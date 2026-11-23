#!/usr/bin/env bash
# Compute an integration commit without checking out or running upstream files.
set -euo pipefail

if [[ $# != 7 || $1 != prepare || $2 != --main || $4 != --upstream || $6 != --output ]]; then
    echo 'Usage: upstream-sync.sh prepare --main REF --upstream REF --output NEW_DIRECTORY' >&2
    exit 2
fi

main=$(git rev-parse --verify --end-of-options "$3^{commit}")
upstream=$(git rev-parse --verify --end-of-options "$5^{commit}")
output=$7
mkdir -m 700 -- "$output"

report="$output/report.md"
printf '## DesignCraft upstream integration\n\nFork base: `%s`\n\nUpstream revision: `%s`\n' "$main" "$upstream" > "$report"

write_status() {
    printf '%s\n' "$1" > "$output/status"
    printf '\n%s\n' "$2" >> "$report"
}

append_paths() {
    local title=$1
    shift
    printf '\n### %s\n\n' "$title" >> "$report"
    git -c core.quotePath=true diff --no-renames --name-status "$@" |
        sed -n '1,200s/^/    /p; 201s/.*/    [file list truncated]/p' >> "$report"
}

if git merge-base --is-ancestor "$upstream" "$main"; then
    write_status unchanged 'The upstream revision is already present in the fork history.'
    exit 0
fi

if ! bases=$(git merge-base --all "$main" "$upstream"); then
    write_status blocked 'Histories have no common ancestor. Import the initial upstream baseline in a reviewed bootstrap PR.'
    exit 1
fi

# GitHub runs workflow definitions from same-repository PRs. Require a human to
# review control-plane changes before any such candidate is pushed.
automation_paths=(.github tools/fork ':(glob)**/.gitattributes')
review_paths=(crates/doc crates/engine crates/format crates/ui-egui apps/designcraft
    crates/collab-core crates/designcraft-collab packaging/macos .github tools/fork
    Cargo.toml Cargo.lock ':(glob)**/Cargo.toml')

while IFS= read -r base; do
    append_paths 'Incoming files' "$base" "$upstream" --
    if [[ -n $(git diff --name-only --no-renames "$base" "$upstream" -- "${automation_paths[@]}") ]]; then
        write_status blocked 'Upstream changed repository automation or merge attributes. Review the incoming control-plane changes locally before pushing any branch.'
        append_paths 'Automation and merge attributes requiring review' "$base" "$upstream" -- "${automation_paths[@]}"
        exit 1
    fi
done <<< "$bases"

merge_output="$output/merge.txt"
if git -c core.quotePath=true -c core.attributesFile=/dev/null merge-tree --write-tree "$main" "$upstream" > "$merge_output"; then
    read -r tree < "$merge_output"
else
    result=$?
    if [[ $result != 1 ]]; then
        echo 'git merge-tree failed before producing an integration candidate.' >&2
        exit "$result"
    fi
    write_status conflict 'Git detected a merge conflict. Resolve it in a maintainer-owned branch after inspecting the upstream changes.'
    printf '\n### Merge diagnostics\n\n' >> "$report"
    sed -n '1,200s/^/    /p; 201s/.*/    [diagnostics truncated]/p' "$merge_output" >> "$report"
    exit 1
fi

if [[ -n $(git diff --name-only --no-renames "$main" "$tree" -- "${automation_paths[@]}") ]]; then
    write_status blocked 'The result changes repository automation or merge attributes and requires review before push.'
    append_paths 'Automation and merge attributes requiring review' "$main" "$tree" -- "${automation_paths[@]}"
    exit 1
fi

# Identical input commits produce the same candidate so a retry is safe after
# a branch push succeeds but PR creation fails.
commit_date=$(git show -s --format=%cI "$upstream")
candidate=$(GIT_AUTHOR_NAME='CoDesign Upstream Sync' GIT_AUTHOR_EMAIL='upstream-sync@users.noreply.github.com' \
    GIT_COMMITTER_NAME='CoDesign Upstream Sync' GIT_COMMITTER_EMAIL='upstream-sync@users.noreply.github.com' \
    GIT_AUTHOR_DATE="$commit_date" GIT_COMMITTER_DATE="$commit_date" \
    git -c commit.gpgsign=false commit-tree "$tree" -p "$main" -p "$upstream" \
        -m "Merge DesignCraft upstream $upstream")
printf '%s\n' "$candidate" > "$output/candidate"
write_status ready 'Git merged both parents cleanly. Fork CI and maintainer review are required before integration.'
append_paths 'Changes relative to the fork base' "$main" "$candidate" --
if [[ -n $(git diff --name-only --no-renames "$main" "$candidate" -- "${review_paths[@]}") ]]; then
    append_paths 'Compatibility and security review paths' "$main" "$candidate" -- "${review_paths[@]}"
fi
