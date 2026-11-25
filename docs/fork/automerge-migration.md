# Automerge migration for shared books

The target is a 200-page native document with chapter-length threaded stories and many distinct illustrations. A designer should be able to open an invitation, start typing or paste from another document, keep working through a disconnect, and see the same result as the other designers after reconnecting. A saved native document remains usable as a standalone Collate file.

## Replication boundary

- Each edited story is a separate Automerge document. Unicode text and formatting are merged at that level and projected into Collate's native stories. Opening or editing a chapter does not require initializing every other chapter's Automerge state. The native document remains the layout, composition, and export model.
- Room transactions attribute each Automerge change to the authenticated peer. The CRDT actor stored in a payload is checked against that identity. A transaction is staged, validated and applied atomically before it becomes eligible for durable acknowledgement. Replay reconstructs the same projected content from the original story seed and accepted changes.
- Host validation and ordered commits preserve structural invariants for pages, frames, layer moves and resource imports. A room must tie an Automerge story to a stable identity across those structural edits. Concurrent text edits to an existing story can merge independently of order. Editing a story concurrently with deleting its last frame requires an explicit recovery policy for the author's edits.
- CRDT state excludes image bytes, local filesystem paths and presence. Positions for text selections and comments need stable story anchors. Existing text comments track the original selected characters, so a replacement must preserve that behavior through deletion. Author undo should preserve other authors' edits, including a concurrent format on surviving text.

## Cutover gates

1. Verify the native story projection with concurrent independent edits, offline replay, styled cross-document paste, large threaded stories, and local checkpoints. Keep the legacy room reader available until there is a verified conversion from a confirmed revision.
2. Connect an incrementally stored change stream to authenticated desktop and unattended rooms. Show local edits immediately, queue them durably, and reconcile them after reconnect without blocking input on an acknowledgement. Complete cursor, comment, and author undo parity before changing the default. Treat a new room protocol as a compatibility boundary. Existing rooms must remain recoverable without discarding their history.
3. Open the room from the native model, asset manifest and small requested previews. Fetch originals separately from text and layout updates. Keep the existing SHA-256 and length verification. Exercise cold join, edits during a throttled large transfer, a disconnect, recovery and a late peer against a generated 200-page illustrated book. Also check export with both present and missing originals. Current room ceilings and its full-history write path need replacement or a measured bound for this workload.

Collate and its room server use wire protocol 17 for new rooms. New invitations identify the required protocol. Existing invitations and room directories keep protocol 15. Earlier experimental macOS journals migrate when reopened. A guest needs an updated build to join a protocol 17 room.

Guest story edits are visible immediately. Collate sends successive changes to the same story without waiting for each reply, and a guest with a saved room on macOS can edit stories while offline. Its encrypted outbox keeps local drafts across restarts. Confirmed history uses sealed segments and a small writable tail that records acknowledgements with the remaining outbox. A development room retains up to 16,384 edits or 64 MiB of edit metadata.

Layout changes and saving over the shared file wait for the queue to drain. If a remote change prevents reapplying the draft, the guest can save a copy of the visible draft and can recover the queued insertions after a restart. New Automerge stories track the original characters in each comment. Undo and redo can restore those characters, and undoing one author's edit preserves other authors' overlapping edits. Existing protocol-16 stories keep their original comment and editing behavior.

Invited guests can open a native layout with previews for large raster artwork in the original document while verified originals transfer in the background. Small images and PDFs arrive with the initial document. Saving a native copy and exporting artwork require the originals. Artwork placed after the room starts still travels with its edit.

For a document with linked artwork, **Share a copy with artwork…** verifies the local originals and opens a new native document for the room. Missing or changed files need to be resolved before sharing. The original document keeps its links and its saved path.

Run the native book protocol test with `cargo test --locked -p designcraft-ui-egui --lib --features automerge-rooms collaboration::tests::book::`. The same test without the feature exercises the default protocol.

Before starting or joining an Automerge room, each designer can use **Share Document → Shared artwork folder** to choose the same synced project folder on their machine. A locally synced Dropbox folder works. When the host prepares the invitation download, Collate publishes large raster originals to a content-addressed folder there. A guest tries that folder first, checks the exact length and SHA-256, and falls back to the host when the folder needs more time. Each room uses the local folder selected when it was opened.
