# Integrating DesignCraft upstream

Collate's public history starts with a source snapshot. [`tools/fork/upstream-revision`](../../tools/fork/upstream-revision)
records the exact DesignCraft revision represented in `main`. The [upstream delta](upstream-delta.md)
describes the fork's changes. The studio repo retains the original merge history and
private macOS release workflow.

## Automatic integration

The **Upstream sync** workflow runs at 05:47 UTC when enabled. It also supports manual
dispatch on `main`, with publication off by default. It fetches
`storytold/designcraft:main` and makes a three-way merge using the recorded revision
as its base. Planning uses the current trusted workflow and tooling.

A clean import produces a commit whose parent is the current Collate `main` and whose
tree records the incoming revision. The pull request identifies both revisions and
lists the changed files and compatibility paths. This gives the public snapshot a
traceable integration history without attaching the underlying upstream object graph.
The public **Workspace quality gate** and **macOS application** checks are required
before merging into `main`.

The planner reports conflicts, non-descendant upstream revisions, and changes to
workflow files, sync tooling, Git merge attributes, Git submodule metadata or the
upstream brand directory without pushing a branch. An identical revision pair reuses
its existing pull request, including a closed one. If only the push succeeded, a retry
reuses that exact branch. A later revision or a new Collate base gets its own PR.

## GitHub setup

1. Register a GitHub App for Collate sync and install it on this repository alone.
   Grant **Contents: write** and **Pull requests: write**. Leave workflow-write permission unset.
   The production release pipeline lives in the separate studio repository.
2. Create an `upstream-sync` environment that allows deployments from `main` only.
   Add the App's client ID as `UPSTREAM_SYNC_APP_CLIENT_ID` and its private key as the
   environment secret `UPSTREAM_SYNC_APP_PRIVATE_KEY`.
3. Dispatch **Upstream sync** with publication off to inspect the result. Once the App
   is configured, set the repository variable `UPSTREAM_SYNC_ENABLED` to `true`.
   The workflow mints its scoped installation token only for a ready candidate that
   is authorized to publish.

CI uses read-only repository access. Apple and Sparkle credentials, signed installers
and the update feed belong to the private studio release pipeline.

## Resolve a conflict

Start a branch from the reviewed public base and inspect the exact upstream delta.

```sh
git fetch origin main
git fetch --no-tags https://github.com/storytold/designcraft.git \
  refs/heads/main:refs/remotes/upstream/main
git switch -c sync/reviewed-upstream origin/main
base=$(git show HEAD:tools/fork/upstream-revision)
incoming=$(git rev-parse upstream/main)
git diff --binary "$base" "$incoming" --
```

Apply the reviewed changes and resolve any conflicts on that branch. Preserve Collate's
attribution, app identity and CI boundaries. Update the recorded revision as part of
the same commit as the imported changes. Open a PR with the upstream revision and the
resolution, then review the two required hosted checks.

```sh
bash tools/fork/test-upstream-sync.sh
bash tools/fork/test-upstream-publish.sh
cargo xtask ci
```

The shell fixtures cover both ordinary related histories and independent public
snapshots. They check clean and conflicting integrations, protected paths, candidate
ancestry, metadata consistency, new revisions, recovery after PR creation fails
and duplicate and stale candidate handling.
