#!/usr/bin/env bash
# Run from the trusted main checkout, with the installation token in GH_TOKEN.
set -euo pipefail

if [[ $# != 3 ]]; then
    echo 'Usage: publish-upstream-sync.sh REPOSITORY MAIN_SHA RESULT_DIRECTORY' >&2
    exit 2
fi
repository=$1
main=$2
result=$3
[[ $(cat "$result/status") == ready ]]
candidate=$(git rev-parse --verify --end-of-options "$(cat "$result/candidate")^{commit}")
upstream=$(git rev-parse --verify --end-of-options "$(cat "$result/upstream")^{commit}")
parents=$(git show -s --format=%P "$candidate")
revision_file=tools/fork/upstream-revision
if baseline=$(git show "$main:$revision_file" 2>/dev/null); then
    candidate_revision=$(git show "$candidate:$revision_file")
    if [[ "$parents" != "$main" || "$candidate_revision" != "$upstream" || "$baseline" == "$upstream" ]] ||
       ! git merge-base --is-ancestor "$baseline" "$upstream"; then
        echo 'The snapshot candidate does not match its reviewed base and upstream revision.' >&2
        exit 1
    fi
    operation=Import
else
    if [[ "$parents" != "$main $upstream" ]]; then
        echo 'The integration commit does not have the expected fork and upstream parents.' >&2
        exit 1
    fi
    operation=Merge
fi
branch="sync/upstream-${upstream:0:16}-${main:0:16}"

# Authenticate to GitHub without storing the token in the repository or the remote URL.
github_git() {
    git -c credential.helper= -c 'credential.helper=!gh auth git-credential' "$@"
}

existing=$(gh pr list --repo "$repository" --head "$branch" --base main --state all \
    --limit 1 --json url --jq '.[0].url // empty')
if [[ -n "$existing" ]]; then
    printf 'This pair of revisions already has a pull request: %s\n' "$existing"
    exit 0
fi

remote_main=$(github_git ls-remote --exit-code origin refs/heads/main)
if [[ ${remote_main%%$'\t'*} != "$main" ]]; then
    echo 'Fork main advanced during preparation. The next synchronization run will recompute the candidate.'
    exit 0
fi

remote_candidate=$(github_git ls-remote origin "refs/heads/$branch")
if [[ -z "$remote_candidate" ]]; then
    # An empty expected value permits creation only, including if another actor raced the lookup.
    github_git push --force-with-lease="refs/heads/$branch:" origin "$candidate:refs/heads/$branch"
elif [[ ${remote_candidate%%$'\t'*} != "$candidate" ]]; then
    echo "Candidate branch contains a different commit: $branch" >&2
    exit 1
fi

# Preserve existing PRs and review history. A new revision or base gets a new PR.
gh pr create --repo "$repository" --base main --head "$branch" \
    --title "$operation DesignCraft upstream ${upstream:0:12}" --body-file "$result/report.md"
