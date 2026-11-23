# Collate

Collate is an open-source collaborative desktop publishing fork of DesignCraft. Sharing uses the peer transport adapted from Repro With Friends.

## Share a document

1. Open a saved `.designcraft` file on the hosting Mac. A room can include bounded embedded PDF, PNG, JPEG, GIF and WebP artwork. Linked files and data merges need a standalone document.
2. On the hosting Mac, select **Share…**, enter a name and select **Start sharing**. Leave internet relay off on the same network. Enable it before starting whenever collaborators need the iroh relay.
3. Select **Copy invitation code** and share it privately. Guests can select **Join shared document…** on the start screen, enter the code and choose **Join and receive document**. Collate opens the verified seed in a new tab. To use an identical saved copy or resume a previously saved room, choose **Join with open document**.
4. Draw, move, resize and rotate unassigned frames with the standard tools. Supported edits include fill, stroke, basic appearance, layer assignment and stacking order. The host confirms each edit. Comments can attach to a selected frame or a page. Replies and resolution travel with the room history.
5. Save normally to keep the confirmed document in the DesignCraft format. Use **Save a Copy** to write a separate snapshot. On macOS, reopen the original local file path to recover its room. A restarted host shares a fresh invitation. Guests can use **Reconnect** without discarding a queued edit. Guests who download a seed should save a local copy to enable encrypted room recovery.

Draw a new frame with the Type tool or click an unassigned frame to start a story. The creator's caret follows the confirmed frame, and typing entered while confirmation is pending is buffered for that frame. Simple stories, including stories in already threaded frames, support concurrent insertion, deletion, and basic character and paragraph formatting. Apply an existing paragraph style or change alignment, indents, spacing and hyphenation. A single unthreaded text frame with a simple story can be deleted. If a deletion wins a conflict with typing, the author can use **Copy queued typing** in the Share window to salvage the submitted and buffered text. Changing frame threads remains a standalone edit.

Use **Insert Pages…** or **Delete Pages** in standard facing-page and single-sided documents. Retained frames and selections follow their pages as spreads change. Deleting a page can remove its unreferenced supported frames, and comments on removed pages or frames remain available as orphaned threads. Page edits are declined if they would move artwork to a different page or change where a bookmark or page link points.

Use **File > Place** or drag one PDF, PNG, JPEG, GIF or WebP file of up to 2 MiB onto the canvas. Select an existing frame to replace its artwork with **File > Place**, or drop the file on the frame. Without a selected frame, **File > Place** puts the artwork at the first page margin. For a multi-page PDF, **File > Place** opens the standard page and crop options. A drop uses the first page and its default crop. Room members receive the embedded file and its filename, including the original vector data for PDFs. The invitation includes compatible artwork already embedded in the document. Linked files, other artwork formats and custom spread editing remain pending. Partially supported edits are rejected as a unit.

Use the existing **Layers** panel to create or reorder shared layers, change options such as name, visibility and lock, and merge or remove layers. Collate rejects removal that would leave layer guides, unsupported nested or anchored content, or protected frame references. Duplicating a populated layer remains a standalone edit. Each participant chooses their own active layer.

Shared undo and redo apply to your own simple text edits while preserving other collaborators' typing, deletions and formatting. They also cover frame properties, order and comment resolution when no teammate has since changed the same target. Artwork and structural edits remain outside shared undo.

Font files stay on each Mac. Collate uses DesignCraft's local missing-font fallback, which can change line breaks and output. Participants should install the same licensed fonts before reviewing page composition or exporting. Embed supported linked images before starting a room so their appearance travels with the document.

## Develop

```sh
cargo run --locked -p designcraft
cargo test --locked -p designcraft-collab
cargo test --locked -p designcraft-collab-core --all-features
cargo test --locked -p designcraft-engine --test it deferred_edits
cargo test --locked -p designcraft-ui-egui --lib collaboration::tests
cargo xtask ci
bash tools/fork/test-upstream-sync.sh
bash tools/fork/test-upstream-publish.sh
bash tools/fork/test-document-roundtrip.sh
```

On macOS, run the native journal test with `cargo test --locked -p designcraft-collab-core --features secure-journal --lib journal::tests:: -- --include-ignored`. A disposable Keychain lets a second process reopen and update the encrypted file. The test also verifies that a missing key leaves the existing ciphertext intact.

The macOS CI job also reopens a shared room in a separate test process with a disposable
Keychain. To run the same test locally:

```sh
cargo test --locked -p designcraft-ui-egui --lib collaboration::tests::restart::native_room_recovery_reconnects_after_process_exit -- --ignored --exact
```

The test verifies both peer identities, the restored document and comment, resend of an
unacknowledged edit, and a new edit saved by the guest.

- [Architecture](architecture.md)
- [Collaboration protocol](collaboration-protocol.md)
- [Integrating DesignCraft upstream](upstream-sync.md)
- [Changes to DesignCraft](upstream-delta.md)
- [Security boundaries](security.md)
