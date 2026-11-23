#!/usr/bin/env bash
# Exercise the public CLI and the existing archive, renderer and PDF exporter.
set -euo pipefail

work=$(mktemp -d "${TMPDIR:-/tmp}/codesign-document-test.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT

cargo build --locked -q -p designcraft-cli
cli="${CARGO_TARGET_DIR:-target}/debug/designcraft-cli"

"$cli" run \
    --cmd 'file.new={}' \
    --cmd 'frame.create={"rect":[20,30,120,180],"content":"unassigned"}' \
    --cmd 'object.fill={"swatch":"[Black]"}' \
    --cmd 'transform.move={"dx":37,"dy":19}' \
    --export "$work/initial.designcraft"

"$cli" run \
    --in "$work/initial.designcraft" \
    --export "$work/reopened.designcraft" \
    --export "$work/reopened.png" \
    --export "$work/reopened.pdf"

python3 - "$work" <<'PY'
import json
from pathlib import Path
import sys
from zipfile import ZipFile

work = Path(sys.argv[1])

def document(name):
    with ZipFile(work / name) as archive:
        return json.loads(archive.read("document.json"))

initial = document("initial.designcraft")
reopened = document("reopened.designcraft")
assert len(initial["spreads"][0]["items"]) == 1
item = initial["spreads"][0]["items"][0]
assert item["fill"]["swatch"] == "[Black]"
assert item["xf"][-2:] == [37, 19]
assert reopened["spreads"] == initial["spreads"]
assert reopened["stories"] == initial["stories"]
assert (work / "reopened.png").read_bytes().startswith(b"\x89PNG\r\n\x1a\n")
assert (work / "reopened.pdf").read_bytes().startswith(b"%PDF-")
print("Native document roundtrip, rendering and PDF export passed.")
PY
