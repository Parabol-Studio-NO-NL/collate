# Collate architecture

Collate keeps the DesignCraft document, composition, rendering and file format. The fork adds a typed collaboration adapter, a Repro-derived native transport and an egui room controller. See the [upstream guide](upstream-sync.md) for the DesignCraft baseline.

| Component | Responsibility |
| --- | --- |
| `collab-core` | iroh peer transport, invitation parsing, authenticated endpoint identities, bounded worker queues and macOS Keychain-backed journal |
| `designcraft-collab` | Constrained object edits, stable ID mapping, semantic patches, property-level conflicts, compensating edits and comments |
| `engine/src/deferred_edits.rs` | Captures approved command and gesture changes for one open document, leaving the confirmed document active until the room accepts the proposal |
| `ui-egui/src/collaboration/` and controller | Host ordering, catch-up, durable journal coordination, participant presence and selection overlays, sharing, comments and local shutdown gating |
| `packaging/macos/codesign` | Independent bundle identity, universal build, Sparkle framework, signing, verification and appcast creation |

A shared drag is rendered by DesignCraft as an interaction preview. Its final before/after pair goes to the adapter. The adapter projects the proposed changes onto a copy of the confirmed document and rejects a draft if that projection cannot reproduce it. The confirmed document is updated on the egui thread. A remote message can request typed operations only and never reaches the normal command or automation dispatcher.

The operation host sequences accepted transactions. A guest can submit one outstanding or offline proposal at a time. The room uses an immutable document seed and hashes it for negotiation. A new guest can request its verified seed over the encrypted room channel. A bounded native archive includes compatible embedded images. The app validates IDs, content hashes and image limits before opening it in a fresh tab. Canonical operations replay from that seed when a peer catches up. Normal application settings, tool state and document views remain local. Snapshots in DesignCraft's ordinary undo stack are cleared after a collaborative document commit because undoing that snapshot could revert work from another participant.

## Persistence boundary

The macOS journal keeps the original seed, accepted operations, actor identity, comments and any single pending proposal in a bounded encrypted record. The local record is scoped by the original canonical file path, participant role and guest host identity. Recovery checks the file creation timestamp and verifies that the reopened document matches one of the room's confirmed revisions before replaying changes. An identical copy placed at the original path can also match. Saving a native file writes the currently confirmed document using the existing format. Use **Save a Copy** for a separate standalone file while the room is open. Preserve the original path when you want to resume the room later.

The app requests durable journal storage before confirming an edit. Keychain or disk errors leave the confirmed state in place. The normal plaintext crash-recovery writer skips a document with encrypted room recovery. Standalone files remain normal local documents. Collaboration on other native targets is currently memory-only. The web build uses the standalone editor.

A room holds one host and up to three guests. Stable peer identities survive reopening on macOS. A room's invitation rotates on host restart. There is no host election and nobody can commit new shared changes while the host is unavailable. Restored guests present their last confirmed revision and replay missing commits before re-sending a pending proposal.
