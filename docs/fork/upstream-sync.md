# Integrating DesignCraft upstream

Collate's public history starts with a source snapshot based on [DesignCraft
`2d9b83b48eee`](https://github.com/storytold/designcraft/commit/2d9b83b48eee2e55bc814ba82b7c1dd8b21fac11).
The [upstream delta](upstream-delta.md) lists the fork's changes. Private studio history
and release automation live in the studio repository.

For an upstream update, fetch `storytold/designcraft:main` and review the changes
between the recorded baseline and the upstream candidate. Apply that patch on a branch
from Collate's `main`, resolve conflicts and update the baseline after the full review.
Keep Collate's licensing notices, the app identity and the separation of build and
release credentials. Review any workflow changes before pushing the candidate.

Open a pull request with the upstream revision, conflicts resolved and local checks run.
The **Workspace quality gate** and **macOS application** jobs validate it on GitHub.
The upstream and PR publication test scripts under `tools/fork/` use disposable
repositories to exercise the Git integration.
