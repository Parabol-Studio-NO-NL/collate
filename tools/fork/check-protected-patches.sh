#!/usr/bin/env bash
# No arguments checks the checkout. BASE TREE inspects Git objects without checking out upstream.
set -euo pipefail

manifest=tools/fork/protected-patches
if [[ $# == 0 ]]; then
    cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.."
    rules=$(cat -- "$manifest")
elif [[ $# == 2 ]]; then
    rules=$(git show "$1:$manifest")
else
    echo 'Usage: check-protected-patches.sh [FORK_BASE CANDIDATE_TREE]' >&2
    exit 2
fi

failed=0
count=0
while IFS=$'\t' read -r path literal; do
    [[ -n "$path" && ${path:0:1} != '#' ]] || continue
    count=$((count + 1))
    if [[ -n "$literal" ]]; then
        if [[ $# == 0 ]]; then
            [[ -f "$path" && ! -L "$path" ]] && grep -Fq -- "$literal" "$path" && continue
        elif git --literal-pathspecs grep --no-textconv -Fq -e "$literal" "$2" -- "$path"; then
            continue
        fi
        printf 'Missing protected hook in %s: %s\n' "$path" "$literal" >&2
    else
        if [[ $# == 0 ]]; then
            [[ -e "$path" && ! -L "$path" ]] && continue
        elif git cat-file -e "$2:$path" 2>/dev/null &&
             git --literal-pathspecs diff --quiet --no-ext-diff --no-textconv "$1" "$2" -- "$path"; then
            continue
        fi
        printf 'Missing or changed fork module: %s\n' "$path" >&2
    fi
    failed=1
done <<< "$rules"
if [[ $count == 0 ]]; then
    echo 'The fork patch inventory is empty.' >&2
    exit 1
fi
exit "$failed"
