#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
publish_script="$script_dir/publish-upstream-sync.sh"

# Synthetic history and a local bare origin. The Gh fixture never makes API calls.
for variable in $(git rev-parse --local-env-vars); do
    unset "$variable"
done
unset GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_ALLOW_PROTOCOL=file

fixture=$(mktemp -d "${TMPDIR:-/tmp}/codesign-publish-test.XXXXXXXX")
trap 'rm -rf -- "$fixture"' EXIT
mkdir -- "$fixture/bin" "$fixture/repository"
export PATH="$fixture/bin:$PATH"
export SYNC_TEST_REPOSITORY='fixture/codesign'
cd -- "$fixture/repository"

cat > "$fixture/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

[[ ${1:-} == pr ]] || exit 90
command=$2
shift 2
repo= base= head= state= title= body=
while (( $# )); do
    case "$1" in
        --repo) repo=$2 ;;
        --base) base=$2 ;;
        --head) head=$2 ;;
        --state) state=$2 ;;
        --title) title=$2 ;;
        --body-file) body=$2 ;;
        --limit|--json|--jq) ;;
        *) exit 91 ;;
    esac
    shift 2
done
[[ "$repo" == "$SYNC_TEST_REPOSITORY" && "$base" == main && "$head" == sync/upstream-* ]] || exit 92

case "$command" in
    list)
        [[ "$state" == all ]] || exit 93
        if [[ -f "$SYNC_TEST_MOCK/$head.url" ]]; then
            cat -- "$SYNC_TEST_MOCK/$head.url"
        fi
        ;;
    create)
        [[ -n "$title" && -s "$body" ]] || exit 94
        [[ -n $(git ls-remote --exit-code origin "refs/heads/$head") ]] || exit 95
        printf 'create\n' >> "$SYNC_TEST_MOCK/attempts"
        if [[ "${SYNC_TEST_FAIL_CREATE:-}" == 1 ]]; then
            exit 42
        fi
        printf '%s\n' "$title" > "$SYNC_TEST_MOCK/title"
        cp -- "$body" "$SYNC_TEST_MOCK/body"
        printf '%s\n' "$head" > "$SYNC_TEST_MOCK/head"
        printf 'https://github.example.invalid/%s/pull/%s\n' "$repo" "${head##*/}" |
            tee "$SYNC_TEST_MOCK/$head.url"
        ;;
    *) exit 96 ;;
esac
STUB
chmod +x "$fixture/bin/gh"

fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

publish() {
    local result=$1
    local expected=$2
    local code=0
    bash "$publish_script" "$SYNC_TEST_REPOSITORY" "$main" "$result" \
        >"$fixture/publish.log" 2>&1 || code=$?
    if [[ $code != "$expected" ]]; then
        cat -- "$fixture/publish.log" >&2
        fail "Publish expected exit $expected, got exit $code"
    fi
}

make_origin() {
    local scenario=$1
    local origin_main=$2
    local origin_path="$fixture/$scenario.git"
    export SYNC_TEST_MOCK="$fixture/$scenario-gh"
    mkdir -p -- "$SYNC_TEST_MOCK/sync"
    git init --quiet --bare "$origin_path"
    git config remote.origin.url "$origin_path"
    git push --quiet origin "$origin_main:refs/heads/main"
}

git init --quiet --initial-branch=main
git config user.name 'Publish Fixture'
git config user.email 'publish-fixture@example.invalid'
git commit --quiet --allow-empty -m 'Common baseline'
git branch upstream
git commit --quiet --allow-empty -m 'Fork-specific change'
main=$(git rev-parse main)

git switch --quiet upstream
printf 'first change\n' > feature.txt
git add feature.txt
git commit --quiet -m 'First upstream change'
upstream=$(git rev-parse HEAD)
result="$fixture/prepared first candidate"
bash "$script_dir/upstream-sync.sh" prepare --main "$main" --upstream "$upstream" --output "$result"
candidate=$(<"$result/candidate")

printf 'second change\n' >> feature.txt
git add feature.txt
git commit --quiet -m 'Second upstream change'
next_result="$fixture/prepared second candidate"
bash "$script_dir/upstream-sync.sh" prepare --main "$main" --upstream HEAD --output "$next_result"
next_candidate=$(<"$next_result/candidate")

make_origin retry "$main"
SYNC_TEST_FAIL_CREATE=1 publish "$result" 42
[[ $(git ls-remote --heads origin 'refs/heads/sync/*' | cut -f1) == "$candidate" ]] ||
    fail 'A failed PR creation must leave the exact candidate recoverable'

publish "$result" 0
head=$(<"$SYNC_TEST_MOCK/head")
[[ $(git ls-remote --heads origin "refs/heads/$head" | cut -f1) == "$candidate" ]] ||
    fail 'PR must use the branch pointing to the prepared merge'
[[ $(<"$SYNC_TEST_MOCK/title") == *"${upstream:0:12}"* ]] ||
    fail 'PR title must identify the upstream revision'
cmp -s "$result/report.md" "$SYNC_TEST_MOCK/body" ||
    fail 'PR must include the complete integration report'

publish "$result" 0
[[ $(wc -l < "$SYNC_TEST_MOCK/attempts") -eq 2 ]] ||
    fail 'An existing PR for this revision pair should suppress duplicate creation'

publish "$next_result" 0
next_head=$(<"$SYNC_TEST_MOCK/head")
[[ $next_head != "$head" && $(git ls-remote --heads origin "refs/heads/$next_head" | cut -f1) == "$next_candidate" ]] ||
    fail 'A newer upstream revision must get its own candidate while the earlier PR is pending'
[[ $(git ls-remote --heads origin "refs/heads/$head" | cut -f1) == "$candidate" ]] ||
    fail 'A newer candidate must leave the earlier review branch intact'

git switch --quiet main
git commit --quiet --allow-empty -m 'Fork advanced since preparation'
advanced_main=$(git rev-parse HEAD)
make_origin stale "$advanced_main"
publish "$result" 0
[[ -z $(git ls-remote --heads origin 'refs/heads/sync/*') && ! -f "$SYNC_TEST_MOCK/attempts" ]] ||
    fail 'Publishing a stale candidate must skip both branch push and PR creation'

make_origin collision "$main"
git push --quiet origin "$main:refs/heads/$head"
publish "$result" 1
[[ $(git ls-remote --heads origin "refs/heads/$head" | cut -f1) == "$main" &&
   ! -f "$SYNC_TEST_MOCK/attempts" ]] ||
    fail 'An existing unexpected branch must remain untouched and receive no PR'

snapshot_baseline=$(git merge-base "$main" "$upstream")
git switch --quiet --orphan snapshot
mkdir -p tools/fork
printf '%s\n' "$snapshot_baseline" > tools/fork/upstream-revision
git add tools/fork/upstream-revision
git commit --quiet -m 'Independent public snapshot'
main=$(git rev-parse HEAD)
snapshot_result="$fixture/prepared snapshot candidate"
bash "$script_dir/upstream-sync.sh" prepare --main "$main" --upstream "$upstream" --output "$snapshot_result"

make_origin snapshot "$main"
publish "$snapshot_result" 0
head=$(<"$SYNC_TEST_MOCK/head")
remote_snapshot=$(git ls-remote --heads origin "refs/heads/$head" | cut -f1)
[[ "$remote_snapshot" == "$(cat "$snapshot_result/candidate")" &&
   "$(git show -s --format=%P "$remote_snapshot")" == "$main" &&
   "$(git show "$remote_snapshot:tools/fork/upstream-revision")" == "$upstream" ]] ||
    fail 'Public PR must use the reviewed snapshot candidate and baseline'
if git merge-base --is-ancestor "$upstream" "$remote_snapshot"; then
    fail 'Snapshot publication exposed raw upstream ancestry'
fi

make_origin snapshot-mismatch "$main"
printf '%s\n' "$snapshot_baseline" > "$snapshot_result/upstream"
publish "$snapshot_result" 1
[[ -z $(git ls-remote --heads origin 'refs/heads/sync/*') && ! -f "$SYNC_TEST_MOCK/attempts" ]] ||
    fail 'A mismatched snapshot revision must not publish a branch or pull request'

printf '%s\n' 'Upstream publication fixture passed.'
