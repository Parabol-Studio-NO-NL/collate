#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
sync_script="$script_dir/upstream-sync.sh"

# Synthetic commits and text files made for this test. No network or user Git configuration.
for variable in $(git rev-parse --local-env-vars); do
    unset "$variable"
done
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null

fixture=$(mktemp -d "${TMPDIR:-/tmp}/codesign-sync-test.XXXXXXXX")
trap 'rm -rf -- "$fixture"' EXIT
mkdir -- "$fixture/repository with spaces"
cd -- "$fixture/repository with spaces"
git init --quiet --initial-branch=main
git config user.name 'Sync Fixture'
git config user.email 'sync-fixture@example.invalid'

commit_all() {
    git add --all
    git commit --quiet -m "$1"
}

fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

prepare() {
    local case_name=$1
    local ref=$2
    local expected_status=$3
    local expected_exit=$4
    local exit_status=0
    output="$fixture/$case_name"

    bash "$sync_script" prepare --main main --upstream "$ref" --output "$output" \
        >"$fixture/prepare.log" 2>&1 || exit_status=$?
    if [[ "$exit_status" -ne "$expected_exit" || ! -f "$output/status" ]]; then
        cat -- "$fixture/prepare.log" >&2
        fail "$case_name: expected exit $expected_exit and a status file, got exit $exit_status"
    fi
    local actual_status
    actual_status=$(<"$output/status")
    [[ "$actual_status" == "$expected_status" ]] ||
        fail "$case_name: expected status $expected_status, got $actual_status"
}

assert_report_contains() {
    grep -Fq -- "$1" "$output/report.md" ||
        fail "${output##*/}: report is missing $1"
}

assert_no_candidate() {
    [[ ! -s "$output/candidate" ]] ||
        fail "${output##*/}: a failed or unnecessary sync produced a candidate"
}

mkdir -p .github/workflows crates/doc
printf 'upstream baseline\n' > .github/workflows/ci.yml
printf 'original shared line\n' > shared.txt
printf 'original local file\n' > local.txt
commit_all 'Common upstream baseline'
git branch initial-upstream

printf 'fork validation\n' > .github/workflows/ci.yml
printf 'fork edit to shared line\n' > shared.txt
printf 'fork-only content\n' > fork.txt
commit_all 'Fork-specific changes'
main=$(git rev-parse HEAD)

git switch --quiet -c upstream-doc initial-upstream
printf 'new document schema\n' > crates/doc/schema.txt
printf 'upstream release note\n' > 'release notes.txt'
commit_all 'Independent upstream changes'
upstream=$(git rev-parse HEAD)

git switch --quiet -c upstream-next
printf 'newer upstream release note\n' >> 'release notes.txt'
commit_all 'New upstream changes while previous candidate is pending'
upstream_next=$(git rev-parse HEAD)

git switch --quiet -c upstream-conflict initial-upstream
printf 'upstream edit to shared line\n' > shared.txt
commit_all 'Conflicting upstream change'

git switch --quiet -c upstream-workflow upstream-doc
printf 'unreviewed workflow\n' > .github/workflows/new-ci.yml
commit_all 'Add upstream workflow'

git switch --quiet -c upstream-tool upstream-doc
mkdir -p tools/fork
printf 'unreviewed orchestration\n' > tools/fork/runner.sh
commit_all 'Add upstream orchestration'

git switch --quiet main
printf 'local staged content\n' > local.txt
git add local.txt
printf 'local unstaged content\n' >> local.txt
printf 'local untracked content\n' > untracked.txt
git for-each-ref --format='%(refname) %(objectname)' > "$fixture/refs-before"
git worktree list --porcelain > "$fixture/worktrees-before"
git diff --binary > "$fixture/worktree-before"
git diff --cached --binary > "$fixture/index-before"
git status --porcelain=v1 --untracked-files=all > "$fixture/status-before"
untracked_hash=$(git hash-object untracked.txt)

# The common upstream commit is already integrated, even while the fork has newer commits.
prepare already-integrated initial-upstream unchanged 0
assert_no_candidate

# A merge includes both histories and retains changes that exist only on the fork.
prepare merge upstream-doc ready 0
candidate=$(<"$output/candidate")
[[ "$(git show -s --format=%P "$candidate")" == "$main $upstream" ]] ||
    fail 'merge: candidate must have the fork and upstream commits as parents'
git diff --quiet "$main" "$candidate" -- .github/workflows/ci.yml fork.txt shared.txt ||
    fail 'merge: candidate changed fork-owned content'
git diff --quiet "$upstream" "$candidate" -- crates/doc/schema.txt 'release notes.txt' ||
    fail 'merge: candidate lost upstream content'
assert_report_contains "$upstream"
assert_report_contains "crates/doc/schema.txt"
assert_report_contains "release notes.txt"

prepare same-merge upstream-doc ready 0
[[ "$(git rev-parse "${candidate}^{tree}")" == "$(git rev-parse "$(<"$output/candidate")^{tree}")" ]] ||
    fail 'same-merge: preparing the same pair changed the merged tree'

# An unmerged candidate must not hide newly available upstream changes.
prepare newer-merge upstream-next ready 0
newer_candidate=$(<"$output/candidate")
[[ "$(git show -s --format=%P "$newer_candidate")" == "$main $upstream_next" ]] ||
    fail 'newer-merge: candidate must incorporate the newer upstream commit'
git diff --quiet "$upstream_next" "$newer_candidate" -- 'release notes.txt' ||
    fail 'newer-merge: candidate lost the newer upstream content'

# A conflicted or automation-sensitive change cannot become a pushable merge candidate.
prepare conflict upstream-conflict conflict 1
assert_no_candidate
assert_report_contains "shared.txt"

prepare workflow-change upstream-workflow blocked 1
assert_no_candidate
assert_report_contains ".github/workflows/new-ci.yml"

prepare tool-change upstream-tool blocked 1
assert_no_candidate
assert_report_contains "tools/fork/runner.sh"

# Checking candidates is observational with respect to the caller's branch, index and files.
[[ "$(git symbolic-ref --short HEAD)" == main ]] || fail 'prepare changed the checked-out branch'
git for-each-ref --format='%(refname) %(objectname)' | cmp -s "$fixture/refs-before" - ||
    fail 'prepare changed a repository ref'
git worktree list --porcelain | cmp -s "$fixture/worktrees-before" - ||
    fail 'prepare left a scratch worktree registered'
git diff --binary | cmp -s "$fixture/worktree-before" - ||
    fail 'prepare changed unstaged edits'
git diff --cached --binary | cmp -s "$fixture/index-before" - ||
    fail 'prepare changed staged edits'
git status --porcelain=v1 --untracked-files=all | cmp -s "$fixture/status-before" - ||
    fail 'prepare changed the caller working tree status'
[[ "$(git hash-object untracked.txt)" == "$untracked_hash" ]] ||
    fail 'prepare changed an untracked file'

printf '%s\n' 'Upstream sync fixture passed.'
