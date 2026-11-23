#!/usr/bin/env bash
# Generate a signed single-release appcast using the bundled app metadata.
set -euo pipefail
umask 077

archive="${1:?usage: appcast.sh ARCHIVE NOTES DOWNLOAD_URL_PREFIX CHANNEL}"
notes="${2:?Specify release notes in Markdown}"
prefix="${3:?Specify the HTTPS download URL prefix}"
channel="${4:?Specify stable or beta}"
sparkle="${SPARKLE_FRAMEWORK_PATH:?Set SPARKLE_FRAMEWORK_PATH}"
: "${CODESIGN_SPARKLE_PUBLIC_KEY:?Set CODESIGN_SPARKLE_PUBLIC_KEY}"

[[ "$prefix" == https://*/ && "$channel" =~ ^(stable|beta)$ ]]

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
base="$(python3 - "$archive" "$prefix" "$channel" "$CODESIGN_SPARKLE_PUBLIC_KEY" <<'PY'
import plistlib
import re
import sys
from zipfile import BadZipFile, ZipFile

archive, prefix, channel, public_key = sys.argv[1:]
try:
    with ZipFile(archive) as bundle:
        entry = bundle.getinfo("Collate.app/Contents/Info.plist")
        if entry.file_size > 65536:
            raise ValueError("bundle metadata exceeds 64 KiB")
        info = plistlib.loads(bundle.read(entry))
except (BadZipFile, KeyError, OSError, ValueError, plistlib.InvalidFileException) as error:
    sys.exit(f"Cannot read Collate release metadata: {error}")

if not isinstance(info, dict) or info.get("CFBundleIdentifier") != "com.larshoie.codesign":
    sys.exit("The release archive must contain a Collate application.")
if info.get("CoDesignUpdateChannel") != channel:
    sys.exit("The release archive belongs to a different update channel.")
if info.get("SUPublicEDKey") != public_key or info.get("SURequireSignedFeed") is not True:
    sys.exit("The release archive does not require a feed signed with the configured Collate key.")

version = info.get("CoDesignVersion")
version_pattern = r"[0-9]+\.[0-9]+\.[0-9]+"
if channel == "beta":
    version_pattern += r"-beta\.[1-9][0-9]*"
if not isinstance(version, str) or not re.fullmatch(version_pattern, version):
    sys.exit("The release version must match the selected update channel.")
feed = info.get("SUFeedURL")
feed_suffix = f"/{channel}/appcast.xml"
if not isinstance(feed, str) or not feed.endswith(feed_suffix):
    sys.exit("The packaged appcast URL must match the selected update channel.")
base = feed.removesuffix(feed_suffix)
if prefix != f"{base}/{channel}/releases/codesign-v{version}/":
    sys.exit("The download URL must match the packaged appcast and release version.")
print(base)
PY
)"
if [[ "$(python3 "$here/private-update-base.py" "$base")" != "$base" ]]; then
  echo "The packaged appcast must use the canonical private update base URL." >&2
  exit 1
fi
scratch="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/codesign-appcast.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
dir="$scratch/updates"
mkdir "$dir"
name="$(basename "$archive")"
cp "$archive" "$dir/$name"
cp "$notes" "$dir/${name%.zip}.md"
args=()
if [[ "$channel" == beta ]]; then args+=(--channel beta); fi

sparkle_sign() {
  local executable="$1"
  shift
  if [[ -n "${CODESIGN_SPARKLE_KEYCHAIN_ACCOUNT:-}" ]]; then
    "$sparkle/bin/$executable" --account "$CODESIGN_SPARKLE_KEYCHAIN_ACCOUNT" "$@"
  else
    printf '%s' "${SPARKLE_PRIVATE_KEY:?Set SPARKLE_PRIVATE_KEY or CODESIGN_SPARKLE_KEYCHAIN_ACCOUNT}" |
      "$sparkle/bin/$executable" --ed-key-file - "$@"
  fi
}

sparkle_sign generate_appcast --download-url-prefix "$prefix" --embed-release-notes --maximum-deltas 0 \
  -o "$dir/appcast.xml" ${args[@]+"${args[@]}"} "$dir"
sparkle_sign sign_update --verify "$dir/appcast.xml"

python3 - "$CODESIGN_SPARKLE_PUBLIC_KEY" "$dir/appcast.xml" "$scratch" <<'PY'
import base64
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

key, feed, directory = sys.argv[1:]
enclosure = ET.parse(feed).find("./channel/item/enclosure")
signature = enclosure.attrib["{http://www.andymatuschak.org/xml-namespaces/sparkle}edSignature"]
directory = Path(directory)
directory.joinpath("archive.sig").write_bytes(base64.b64decode(signature, validate=True))
# RFC 8410 SubjectPublicKeyInfo for an Ed25519 public key.
directory.joinpath("public.der").write_bytes(bytes.fromhex("302a300506032b6570032100") + base64.b64decode(key, validate=True))
PY
openssl pkeyutl -verify -pubin -keyform DER -inkey "$scratch/public.der" \
  -rawin -in "$archive" -sigfile "$scratch/archive.sig"
mv "$dir/appcast.xml" "$(dirname "$archive")/appcast.xml"
