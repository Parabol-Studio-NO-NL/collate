# Collate

Collate is an open-source collaborative desktop publishing fork of DesignCraft. Sharing uses the peer transport adapted from Repro With Friends.

## Share a document

1. Open a saved `.designcraft` file on the hosting Mac. A room can include bounded embedded PDF, PNG, JPEG, GIF and WebP artwork. Linked files and data merges need a standalone document.
2. On the hosting Mac, select **Share** in the top bar, enter a name and select **Start sharing**. Leave internet relay off on the same network. Enable it before starting whenever collaborators need the iroh relay.
3. Select **Copy invitation code** and share it privately. Guests can select **Join shared document…** on the start screen, enter the code and choose **Join and receive document**. Collate opens the verified seed in a new tab. To use an identical saved copy or resume a previously saved room, choose **Join with open document**.
4. Draw, move, resize and rotate unassigned frames with the standard tools. Supported edits include fill, stroke, basic appearance, layer assignment and stacking order. The host confirms each edit. Comments can attach to a page, a selected frame, or up to 512 selected Unicode characters in a supported text story. Open **Share** with the text selected to post. Choose **Select text** on a thread to return to its surviving selection. Replies and resolution travel with the room history.
5. Save normally to keep the confirmed document in the DesignCraft format. Use **Save a Copy** to write a separate snapshot. On macOS, reopen the original local file path to recover its room. A restarted host shares a fresh invitation. Guests can use **Reconnect** without discarding a queued edit. Guests who download a seed should save a local copy to enable encrypted room recovery.

Draw a new frame with the Type tool or click an unassigned frame to start a story. The creator's caret follows the confirmed frame, and typing entered while confirmation is pending is buffered for that frame. Simple stories, including stories in already threaded frames, support concurrent insertion, deletion, and basic character and paragraph formatting. Apply an existing paragraph style or change alignment, indents, spacing, hyphenation and the paragraph rag. A single unthreaded text frame with a simple story can be deleted. If a deletion wins a conflict with typing, the author can use **Copy queued typing** in the Share window to salvage the submitted and buffered text. Changing frame threads remains a standalone edit.

Use **Insert Pages…** or **Delete Pages** in standard facing-page and single-sided documents. Retained frames and selections follow their pages as spreads change. Deleting a page can remove its unreferenced supported frames, and comments on removed pages or frames remain available as orphaned threads. Page edits are declined if they would move artwork to a different page or change where a bookmark or page link points.

Use **File > Place** or drag one PNG, JPEG, GIF, WebP or PDF onto the canvas. A room supports up to 1 GiB of retained artwork, including its starting document and artwork needed to replay earlier edits. An empty room can accept a single 1 GiB image. Raster images also have a limit of 12,000 pixels per side and 16,777,216 pixels per image. The PDF reader accepts files up to 512 MiB. Select an existing frame to replace its artwork with **File > Place**, or drop the file on the frame. Without a selected frame, **File > Place** puts the artwork at the first page margin. For a multi-page PDF, **File > Place** opens the standard page and crop options. A drop uses the first page and its default crop. Room members receive the embedded file and its filename, including the original vector data for PDFs. The invitation includes compatible artwork already embedded in the document. Open **Share** to see progress for a large artwork transfer. Linked files, other artwork formats and custom spread editing remain pending. Partially supported edits are rejected as a unit.

Use the existing **Layers** panel to create or reorder shared layers, change options such as name, visibility and lock, and merge or remove layers. Collate rejects removal that would leave layer guides, unsupported nested or anchored content, or protected frame references. Duplicating a populated layer remains a standalone edit. Each participant chooses their own active layer.

Shared undo and redo apply to your own simple text edits while preserving other collaborators' typing, deletions and formatting. They also cover frame properties, order and comment resolution when no teammate has since changed the same target. Artwork and structural edits remain outside shared undo.

Font files stay on each participant’s computer. When a shared document uses a missing family or style, select **Missing fonts** in the status bar to see which faces this computer needs. Install the same licensed fonts and choose **Reload Fonts**. Until then, fallback fonts may change line breaks and output. For an existing linked image, select it in **Window > Links** and choose **Embed** before sharing. The shared copy contains the artwork and its filename. It drops the local source path that **Unembed** would use. To keep that path in your original, use **File > Save a Copy** and start the room from the copy. Starting a room leaves the file on disk untouched until you save.

### Try collaboration on one Mac

Start sharing a saved test document, then launch another instance from Terminal with its own preferences. On the second Home screen, select **Join shared document…**, enter a different display name, paste the first instance’s invitation and choose **Join and receive document**. Place an image or edit text in either window and check the other. If you save the guest’s copy, choose a different file from the host’s.

```sh
guest_profile="$(mktemp -d)"
open -n -a Collate --env "DESIGNCRAFT_CONFIG_DIR=$guest_profile"
```

### Keep a document available

The standalone room server hosts one document from an office machine or a Linux VPS. People can leave and rejoin with the same invitation while the address remains reachable. The server keeps confirmed edits after all editors quit. See the [Mac and Linux setup guide](room-server.md).

## Shape the paragraph rag

Open **Window > Type & Tables > Rag Options**, or open **Rag Options** from the **Paragraph** panel menu. Select text, place the caret, or select a text frame to work on its paragraphs. The panel can stay beside the document as you edit.

| Pattern | What happens on the page |
| --- | --- |
| Natural rag | Alternates full and shorter lines. Words set the exact line endings. |
| Stepped justification | Fits alternating lines toward the full and inset right edges. |
| Alternating edges | Alternates between a left-aligned and a right-aligned line. |
| Four-stop rag | Repeats four editable right insets. The initial values are 0, 8, 2 and 12 mm. |
| Near-flush | Stretches a nonfinal line when its natural end falls within the chosen distance of the right edge, up to the fitting limits. The initial distance is 3 mm. |
| Contextual indent | Aligns the first line with the previous paragraph’s last visible character in the same column. If the preceding paragraph is blank or the opening cannot fit, the text starts flush. |

The repeating patterns restart in each column or threaded frame. **Off** restores the paragraph’s usual alignment. **Paragraph language** sets the default for the selected paragraphs. Words with an explicit character language keep it. Expand **Hyphenation settings** or **Fitting limits** to adjust how the composer breaks and fits lines.

Choose **New Paragraph Style…** to save the settings for reuse. Existing styles have a **Rag Options** section in **Paragraph Style Options**. Everyone in a shared document needs the same collaboration protocol version. Peers can apply existing styles, edit patterns and insets, change paragraph language, and switch hyphenation on or off. Define advanced fitting and hyphenation settings in a style before starting a room.

## Calculate a modular baseline grid

Open **Layout > Modular Baseline Grid** and choose the number of columns and rows. The column gutter is a distance. The row gap is a count of baseline intervals, so the top and bottom of each module land on the same rhythm as grid-aligned text.

Enter your target leading. With **Fit leading to margins** selected, Collate finds the closest leading that fills the height between the margins with equal rows. Clear it to keep the exact leading and see how much space remains below the last row. The preview shows the resulting module size and actual leading before you apply.

**Apply grid** sets the page columns and row guides and changes the document baseline spacing and origin. Apply a grid to a parent page to reuse it on pages that inherit that layout. A grid applied directly to a document page takes precedence there. Paragraphs with baseline alignment use the new grid. Undo reverts the changes in one step. You can explore the calculator while sharing a document and apply the result in a local document.

## Develop

The **Native Sparkle fixture** runs on changes to the updater and macOS packaging. On a disposable GitHub-hosted macOS runner, it uses a temporary certificate and two ad hoc Collate builds to exercise **Check for Updates** over loopback HTTPS. It verifies the signed appcast and archive, then checks that Sparkle installs and relaunches the newer build from the scratch path.

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

A native smoke test of `f2e4497` ran two Collate editors on the same Mac with
separate loopback control ports and a temporary clipboard. The guest joined
by invitation and received the document. Both editors showed their peer and
agreed on frame geometry after edits from each side. The guest displayed the
host's selection and received the resolved comment. Quit kept an unacknowledged
guest edit queued, and the edit reached both editors when the host resumed.
The guest left and rejoined after a new host edit. The two saved archives
reopened with matching geometry.

- [Architecture](architecture.md)
- [Collaboration protocol](collaboration-protocol.md)
- [Integrating DesignCraft upstream](upstream-sync.md)
- [Changes to DesignCraft](upstream-delta.md)
- [Security boundaries](security.md)
