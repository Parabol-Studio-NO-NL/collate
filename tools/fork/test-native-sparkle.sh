#!/usr/bin/env bash
# Drive the real Collate updater on a disposable macOS Actions runner.
set -euo pipefail
umask 077

if [[ ${GITHUB_ACTIONS:-} != true || ${RUNNER_ENVIRONMENT:-} != github-hosted || ${RUNNER_OS:-} != macOS ]]; then
  echo "Run this fixture on a disposable GitHub-hosted macOS runner." >&2
  exit 2
fi
: "${RUNNER_TEMP:?The test needs a runner-owned temporary directory.}"
: "${SPARKLE_FRAMEWORK_PATH:?Fetch the pinned Sparkle release before running this test.}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
work="$(mktemp -d "$RUNNER_TEMP/collate-native-sparkle.XXXXXX")"
server_pid=""
certificate_fingerprint=""

cleanup() {
  local status=$?
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [[ -n "$certificate_fingerprint" ]]; then
    sudo -n security remove-trusted-cert -d "$work/ca.pem" 2>/dev/null || true
    sudo -n security delete-certificate -Z "$certificate_fingerprint" /Library/Keychains/System.keychain 2>/dev/null || true
  fi
  if [[ $status -ne 0 && -f "$work/server.log" ]]; then
    tail -n 30 "$work/server.log"
  fi
  rm -rf -- "$work"
}
trap cleanup EXIT

# Both the CA and the Sparkle signing seed belong to this single test invocation.
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 1 \
  -subj "/CN=Collate native update fixture" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign" \
  -keyout "$work/ca-key.pem" -out "$work/ca.pem"
openssl req -new -newkey rsa:2048 -nodes -subj "/CN=localhost" \
  -keyout "$work/server-key.pem" -out "$work/server.csr"
cat > "$work/server.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost
EOF
openssl x509 -req -sha256 -days 1 -in "$work/server.csr" -CA "$work/ca.pem" \
  -CAkey "$work/ca-key.pem" -CAcreateserial -extfile "$work/server.ext" -out "$work/server.pem"
certificate_fingerprint="$(openssl x509 -in "$work/ca.pem" -noout -fingerprint -sha1 | cut -d= -f2 | tr -d :)"
if security verify-cert -c "$work/server.pem" -p ssl -n localhost -L > /dev/null 2>&1; then
  echo "The runner already trusts the fresh fixture certificate." >&2
  exit 1
fi
sudo -n security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain "$work/ca.pem"
security verify-cert -c "$work/server.pem" -p ssl -n localhost -L

release="$work/feed/updates/stable/releases/codesign-v90.1.0"
mkdir -p "$release"
python3 tools/fork/native-sparkle-fixture.py serve "$work/feed" "$work/server.pem" "$work/server-key.pem" \
  "$work/port" "$work/requests.log" > "$work/server.log" 2>&1 &
server_pid=$!
for attempt in {1..40}; do
  if [[ -s "$work/port" ]]; then
    break
  fi
  kill -0 "$server_pid"
  sleep 0.25
done
port="$(cat "$work/port")"
[[ "$port" =~ ^[0-9]+$ ]]
base="https://localhost:$port/updates"

unset CODESIGN_SPARKLE_KEYCHAIN_ACCOUNT
openssl genpkey -algorithm ED25519 -out "$work/update-key.pem"
sparkle_key="$(openssl pkey -in "$work/update-key.pem" -outform DER | tail -c 32 | openssl base64 -A)"
export CODESIGN_SPARKLE_PUBLIC_KEY="$(openssl pkey -in "$work/update-key.pem" -pubout -outform DER | tail -c 32 | openssl base64 -A)"
export CODESIGN_UPDATE_FEED="$base/stable/appcast.xml"
export CODESIGN_UPDATE_CHANNEL=stable
export CODESIGN_BUILD_PROFILE=debug
export CODESIGN_UPSTREAM_SHA="$(git rev-parse HEAD)"

arch="$(uname -m)"
packaging=packaging/macos/codesign
CODESIGN_VERSION=90.0.0 CODESIGN_BUILD_NUMBER=1 "$packaging/bundle.sh" "$work/installed" "$arch"
"$packaging/sign-bundle.sh" adhoc "$work/installed" "$work/old.zip"
CODESIGN_VERSION=90.1.0 CODESIGN_BUILD_NUMBER=2 "$packaging/bundle.sh" "$work/new" "$arch"
"$packaging/sign-bundle.sh" adhoc "$work/new" "$release/Collate.zip"
printf 'Disposable Collate HTTPS update fixture.\n' > "$work/notes.md"
SPARKLE_PRIVATE_KEY="$sparkle_key" "$packaging/appcast.sh" \
  "$release/Collate.zip" "$work/notes.md" "$base/stable/releases/codesign-v90.1.0/" stable
unset sparkle_key
mv "$release/appcast.xml" "$work/feed/updates/stable/appcast.xml"

installed="$work/installed/Collate.app"
touch "$work/installed/portable.txt"
defaults write com.larshoie.codesign SUEnableAutomaticChecks -bool false
"$installed/Contents/MacOS/Collate" --check-updater-configuration
xcrun clang -Wall -Wextra -Werror -fobjc-arc -dynamiclib -mmacosx-version-min=12.0 -framework AppKit \
  tools/fork/native-sparkle-click.m -o "$work/click.dylib"
python3 tools/fork/native-sparkle-fixture.py exercise "$installed" "$work" "$work/click.dylib"
