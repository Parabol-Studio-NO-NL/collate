# Collate changes to DesignCraft

Baseline: `storytold/designcraft@2d9b83b48eee2e55bc814ba82b7c1dd8b21fac11`.

| Path | Change | Upstream potential |
| --- | --- | --- |
| `.github/workflows/ci.yml` | Fork-owned pinned CI with the upstream workspace gate and unsigned macOS build. | Fork configuration. |
| Other upstream workflows | Removed mutable reusable workflows for upstream distribution, reporting and platform jobs. Their versions remain available in upstream Git history. | Fork configuration. |
| `.github/CODEOWNERS`, `tools/fork/` | Security-sensitive review paths and local sync fixtures. | Fork configuration. |
| `xtask/src/layers.rs`, `Cargo.toml`, `Cargo.lock` | Register the native transport and typed adapter, and resolve the fork's updater dependencies. Drop unused manifest entries for the upstream updater and the absent `crates/testkit` path that stopped Dependabot. | Fork configuration. |
| `crates/engine/src/lib.rs`, `tooling.rs`, `recovery.rs`, `cmd/file/save.rs`, `tests/file_guard.rs` | Narrow deferred-document edit hooks and recovery limits for a room-bound document. The file access guard registers the app-owned encrypted journal. | Generic integration point after review. |
| `crates/engine/src/cmd/file/place.rs`, `crates/ui-egui/src/app/files.rs` | Shared File > Place and drop reuse the native artwork importer and PDF options, then defer the embedded graphic until confirmation. | Fork integration. |
| `crates/doc/src/edit.rs`, `crates/engine/src/cmd/object.rs`, `crates/engine/src/cmd/text/caret.rs`, `crates/engine/src/deferred_edits.rs`, `crates/ui-egui/src/canvas/input.rs` | Reuse native text-frame conversion in a deferred edit, route early typing to the confirmed frame, and keep Page tool selection local to each peer. | The shared conversion helper could be upstreamed. |
| `crates/ui-egui/src/app*`, `canvas.rs`, `chrome/status_bar.rs`, `lib.rs` | Launch the collaboration window, service peer events on the app thread, draw presence overlays and preserve the ordinary close transaction. | Fork integration. |
| `crates/ui-egui/src/updates_ui.rs` | Replace the built-in release source with an injected native updater action. | Generic injection point after review. |
| `crates/ui-egui/src/theme.rs` | Use the bundled Inter face as a fallback for keyboard shortcut glyphs. | Generic macOS display fix. |
| `apps/designcraft/`, `crates/ui-egui/src/about.rs`, `chrome/`, `menus/` | Collate identity, separate profiles, sparkle startup and removal of restricted upstream product marks. | Fork integration. |
