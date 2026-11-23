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

revision_file=tools/fork/upstream-revision
merge_args=(--write-tree)
parents=(-p "$main" -p "$upstream")
operation=Merge
if baseline=$(git show "$main:$revision_file" 2>/dev/null); then
    if [[ ! "$baseline" =~ ^[0-9a-f]+$ || ${#baseline} != "${#main}" ]] ||
       ! git merge-base --is-ancestor "$baseline" "$upstream"; then
        write_status blocked 'The recorded upstream revision is invalid or is outside the fetched upstream history. Review the baseline and the upstream branch before continuing.'
        exit 1
    fi
    if [[ "$baseline" == "$upstream" ]]; then
        write_status unchanged 'The upstream revision is already recorded in the public snapshot.'
        exit 0
    fi
    bases=$baseline
    merge_args+=(--merge-base="$baseline")
    parents=(-p "$main")
    operation=Import
    printf '\nRecorded upstream base: `%s`\n' "$baseline" >> "$report"
else
    if git merge-base --is-ancestor "$upstream" "$main"; then
        write_status unchanged 'The upstream revision is already present in the fork history.'
        exit 0
    fi
    if ! bases=$(git merge-base --all "$main" "$upstream"); then
        write_status blocked 'Histories have no common ancestor. A public snapshot needs a reviewed upstream revision.'
        exit 1
    fi
fi

# GitHub runs workflow definitions from same-repository PRs. Keep non-open
# upstream marks and control-plane changes out of automated public branches.
automation_paths=(.github tools/fork .gitmodules docs/brand ':(glob)**/.gitattributes')
review_paths=(crates/doc crates/engine crates/format crates/ui-egui apps/designcraft
    crates/collab-core crates/designcraft-collab packaging/macos .github tools/fork
    Cargo.toml Cargo.lock ':(glob)**/Cargo.toml')

while IFS= read -r base; do
    append_paths 'Incoming files' "$base" "$upstream" --
    incoming_guarded=$(git diff --name-only --no-renames "$base" "$upstream" -- "${automation_paths[@]}")
    if [[ -n "$incoming_guarded" ]]; then
        write_status blocked 'Upstream changed automation, Git metadata, or restricted brand files. Review the incoming changes locally before pushing any branch.'
        append_paths 'Guarded paths requiring review' "$base" "$upstream" -- "${automation_paths[@]}"
        exit 1
    fi
done <<< "$bases"

merge_output="$output/merge.txt"
if git -c core.quotePath=true -c core.attributesFile=/dev/null merge-tree "${merge_args[@]}" "$main" "$upstream" > "$merge_output"; then
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

merged_guarded=$(git diff --name-only --no-renames "$main" "$tree" -- "${automation_paths[@]}")
if [[ -n "$merged_guarded" ]]; then
    write_status blocked 'The result changes a guarded path and requires review before push.'
    append_paths 'Guarded paths requiring review' "$main" "$tree" -- "${automation_paths[@]}"
    exit 1
fi

if [[ -n "$baseline" ]]; then
    # Install the new baseline into the candidate using a scratch index. Only
    # the resulting tree is published, so raw upstream history stays separate.
    index="$output/snapshot.index"
    revision_blob=$(printf '%s\n' "$upstream" | git hash-object -w --stdin)
    GIT_INDEX_FILE="$index" git read-tree "$tree"
    GIT_INDEX_FILE="$index" git update-index --add --cacheinfo "100644,$revision_blob,$revision_file"
    tree=$(GIT_INDEX_FILE="$index" git write-tree)
    rm -- "$index"
fi

# Identical input commits produce the same candidate so a retry is safe after
# a branch push succeeds but PR creation fails.
commit_date=$(git show -s --format=%cI "$upstream")
candidate=$(GIT_AUTHOR_NAME='Collate Upstream Sync' GIT_AUTHOR_EMAIL='upstream-sync@users.noreply.github.com' \
    GIT_COMMITTER_NAME='Collate Upstream Sync' GIT_COMMITTER_EMAIL='upstream-sync@users.noreply.github.com' \
    GIT_AUTHOR_DATE="$commit_date" GIT_COMMITTER_DATE="$commit_date" \
    git -c commit.gpgsign=false commit-tree "$tree" "${parents[@]}" \
        -m "$operation DesignCraft upstream $upstream")
printf '%s\n' "$upstream" > "$output/upstream"
printf '%s\n' "$candidate" > "$output/candidate"
write_status ready 'Git applied the upstream changes cleanly. Fork CI and maintainer review are required before integration.'
append_paths 'Changes relative to the fork base' "$main" "$candidate" --
if [[ -n $(git diff --name-only --no-renames "$main" "$candidate" -- "${review_paths[@]}") ]]; then
    append_paths 'Compatibility and security review paths' "$main" "$candidate" -- "${review_paths[@]}"
fi
