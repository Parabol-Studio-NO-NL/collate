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
candidate=$(cat "$result/candidate")
[[ $(git rev-parse "$candidate^1") == "$main" ]]
upstream=$(git rev-parse "$candidate^2")
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
    --title "Merge DesignCraft upstream ${upstream:0:12}" --body-file "$result/report.md"
