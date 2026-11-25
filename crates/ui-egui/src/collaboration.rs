//! Native app-thread room controller. Network messages never enter the command registry.

mod attachments;
mod controls;
mod edit_wire;
mod history;
mod join;
mod presence;
mod replay;
mod seed;
mod seed_document;
#[cfg(unix)]
pub mod server;
mod storage;
mod story_sync;
#[cfg(test)]
mod tests;
mod text_selection;
mod typing;

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Weak},
    time::Duration,
};

use designcraft_collab::automerge_stories::StoryTransaction;
use designcraft_collab::{Operation, Replica, RoomProtocol, layers::LayerEdit};
use designcraft_collab_core::{ActorId, Event, MAX_PAYLOAD, Peer, Session as Transport};
use designcraft_doc::{Document, LayerId, PageId, Selection, SpreadRef};
use designcraft_engine::{DeferredEdit, Session};
use serde::{Deserialize, Serialize};
use web_time::Instant;

use self::{
    edit_wire::{SendCursor, WireEdit},
    history::HistoryAction,
    join::{JoinedSeed, OriginalDownloads, PendingJoin, SeedDelivery},
    presence::Presence,
    seed::{SeedAdvertisement, SeedChunk, SeedDownload},
    storage::{OpenRoom, Storage},
};
use crate::DesignApp;

const MAX_QUEUED_STORY_EDITS: usize = 64;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    sequence: u64,
    base: u64,
    edits: Vec<Operation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history_action: Option<HistoryAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    story_transaction: Option<StoryTransaction>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Commit {
    revision: u64,
    actor: ActorId,
    proposal: Proposal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    correction: Option<u64>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "camelCase", deny_unknown_fields)]
enum Message {
    Hello { seed: [u8; 32], revision: u64 },
    CaughtUp { revision: u64 },
    Propose(Proposal),
    Commit(Commit),
    Declined { sequence: u64, artwork_limit: bool },
    RetryArtwork,
    ArtworkReceipt { revision: u64, sequence: u64, digest: [u8; 32] },
    Incompatible,
    RequestSeed,
    RequestOriginal { blob: designcraft_collab::artwork::BlobId },
    SeedOffer(SeedAdvertisement),
    SeedPart(SeedChunk),
    EditOffer(SeedAdvertisement),
    EditPart(SeedChunk),
    Presence { actor: ActorId, state: Presence },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Packet {
    version: u16,
    message: Message,
}

#[cfg(test)]
fn encode(message: Message) -> Result<Vec<u8>, String> {
    encode_for_protocol(message, RoomProtocol::default())
}

fn encode_for_protocol(message: Message, protocol: RoomProtocol) -> Result<Vec<u8>, String> {
    let bytes = encode_packet(message, protocol)?;
    if bytes.len() > MAX_PAYLOAD {
        return Err("The edit is too large to share in one step.".into());
    }
    Ok(bytes)
}

fn encode_packet(message: Message, protocol: RoomProtocol) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&Packet { version: protocol.version(), message }).map_err(|_| "Could not encode the collaboration message.".into())
}

fn encode_edit_for_protocol(message: Message, protocol: RoomProtocol) -> Result<WireEdit, String> {
    let can_chunk = matches!(&message, Message::Propose(proposal) | Message::Commit(Commit { proposal, .. }) if accepts_chunked_edit(proposal));
    let (blobs, artwork_size) = match &message {
        Message::Propose(p) | Message::Commit(Commit { proposal: p, .. }) => (p.blobs()?, p.artwork_size()),
        _ => (attachments::Blobs::new(), 0),
    };
    attachments::total(&blobs)?;
    let bytes = if can_chunk { encode_packet(message, protocol)? } else { encode_for_protocol(message, protocol)? };
    let mut wire = WireEdit::for_room(bytes, protocol)?;
    wire.blobs = blobs;
    wire.artwork_size = artwork_size;
    Ok(wire)
}

fn accepts_chunked_edit(proposal: &Proposal) -> bool {
    proposal.story_transaction.is_some()
        || proposal.edits.iter().any(|edit| edit.changes_artwork() || matches!(edit, Operation::CreateText { .. } | Operation::Text { .. }))
        || matches!(proposal.edits.as_slice(), [Operation::CreateComment { anchor, .. }] if anchor.is_text())
}

enum Mode {
    Host { delivery: BTreeMap<ActorId, Delivery> },
    Guest { ready: bool, hello_sent: bool },
}

#[derive(Clone, Copy)]
struct Delivery {
    sent: usize,
    cursor: SendCursor,
    announced: bool,
    receipt: Option<usize>,
}

struct Room {
    transport: Transport,
    protocol: RoomProtocol,
    #[cfg(unix)]
    server_journal: Option<server::Journal>,
    storage: Storage,
    history: Vec<(Commit, WireEdit)>,
    inverses: Vec<Option<Vec<Operation>>>,
    history_bytes: usize,
    committed_sequences: BTreeMap<(ActorId, u64), usize>,
    history_artwork: u64,
    seed_artwork: u64,
    model_size: Option<(Weak<Document>, usize)>,
    seed_delivery: SeedDelivery,
    originals: OriginalDownloads,
    receiving_edits: BTreeMap<ActorId, SeedDownload>,
    receiving_blobs: BTreeMap<ActorId, attachments::BlobDownload>,
    sending_proposal: Option<(WireEdit, SendCursor)>,
    online: bool,
    has_joined: bool,
    mode: Mode,
    doc_uid: u64,
    seed: [u8; 32],
    replica: Replica,
    local: Option<Peer>,
    host: Option<ActorId>,
    peers: BTreeMap<ActorId, Peer>,
    presence: BTreeMap<ActorId, Presence>,
    invitation: Option<String>,
    next_sequence: u64,
    revision: u64,
    pending: VecDeque<Proposal>,
    pending_story_selection: Option<story_sync::PendingSelection>,
    story_draft: Option<story_sync::Draft>,
    story_queue_blocked: bool,
    story_sender: story_sync::Sender,
    rejected_comment: Option<CommentDraft>,
    frame_entry: Option<typing::FrameEntry>,
    layer_entry: Option<(u64, LayerId)>,
    typing: Option<typing::BufferedTyping>,
    last_send: Option<Instant>,
    last_presence: Option<Instant>,
    error: Option<String>,
    fatal: bool,
    #[cfg(target_os = "macos")]
    recovery_path_checked: Option<String>,
}

struct CommentDraft {
    text: String,
    reply_to: Option<designcraft_collab::SharedId>,
    needs_text_selection: bool,
}

struct RoomConflict {
    doc_uid: u64,
    revision: u64,
    path: String,
    previous: [u8; 32],
    protocol: RoomProtocol,
    focus_start: bool,
}

impl RoomConflict {
    fn applies(&self, session: &Session) -> bool {
        session.active().is_some_and(|state| state.uid == self.doc_uid && state.revision == self.revision && state.path.as_ref() == Some(&self.path))
    }
}

#[derive(Default)]
pub(crate) struct Collaboration {
    pub(crate) open: bool,
    name: String,
    join_code: String,
    relay: bool,
    room: Option<Room>,
    joining: Option<PendingJoin>,
    message: Option<String>,
    room_conflict: Option<RoomConflict>,
    comment: String,
    reply_to: Option<designcraft_collab::SharedId>,
    comment_needs_text_selection: bool,
    confirm_discard: bool,
}

impl DesignApp {
    pub(crate) fn poll_collaboration(&mut self, ctx: &egui::Context) {
        self.poll_seed_download(ctx);
        let Some(room) = self.collaboration.room.as_mut() else { return };
        let Some(st) = self.session.documents().iter().find(|st| st.uid == room.doc_uid) else {
            self.session.defer_document_edits(None);
            self.collaboration.room = None;
            self.collaboration.comment.clear();
            self.collaboration.reply_to = None;
            self.collaboration.comment_needs_text_selection = false;
            self.collaboration.message = Some("The shared document closed.".into());
            return;
        };
        if st.interaction.is_none() {
            #[cfg(target_os = "macos")]
            if let Some(path) = st.path.as_deref() {
                room.enable_recovery(self.session.recovery_dir.as_deref(), path);
            }
            room.poll(&mut self.session);
        }
        if let Some(draft) = room.rejected_comment.take() {
            self.collaboration.comment = draft.text;
            self.collaboration.reply_to = draft.reply_to;
            self.collaboration.comment_needs_text_selection = draft.needs_text_selection;
        }
        room.update_edit_policy(&mut self.session);
        room.flush_typing(&mut self.session);
        ctx.request_repaint_after(Duration::from_millis(40));
    }

    /// Complete a story draft in the input event, before paint and without polling the transport.
    pub(crate) fn apply_local_story_edit(&mut self, id: &str, inside_story: bool) -> Result<(), String> {
        if !designcraft_engine::is_deferred_story_command(id, inside_story) {
            return Ok(());
        }
        let Some(room) =
            self.collaboration.room.as_mut().filter(|room| room.can_draft_story() && self.session.active().is_some_and(|st| st.uid == room.doc_uid))
        else {
            return Ok(());
        };
        let Some(edit) = self.session.take_deferred_edit() else { return Ok(()) };
        let result = room.capture_edit(&mut self.session, edit);
        room.update_edit_policy(&mut self.session);
        if let Err(error) = &result {
            room.error = Some(error.clone());
        }
        result
    }

    pub(crate) fn collaboration_blocks_close(&self) -> bool {
        self.session.has_deferred_edit()
            || self
                .collaboration
                .room
                .as_ref()
                .is_some_and(|room| (!room.pending.is_empty() || room.typing.is_some()) && !self.collaboration_can_resume_document(room.doc_uid))
    }

    pub(crate) fn collaboration_can_resume_document(&self, uid: u64) -> bool {
        self.collaboration.room.as_ref().is_some_and(|room| {
            room.doc_uid == uid
                && room.storage.is_durable()
                && room.story_draft.is_some()
                && !room.story_queue_blocked
                && room.typing.is_none()
                && !self.session.has_deferred_edit()
                && self.session.documents().iter().any(|st| st.uid == uid && st.path.is_some() && st.interaction.is_none())
        })
    }

    pub(crate) fn check_collaboration_close(&self, index: Option<usize>) -> Result<(), String> {
        let Some(room) = &self.collaboration.room else { return Ok(()) };
        let closing = index.and_then(|index| self.session.documents().get(index)).or_else(|| self.session.active());
        if closing.is_some_and(|st| st.uid == room.doc_uid) && self.collaboration_blocks_close() {
            return Err("Wait for the host to confirm your edit before closing the shared document.".into());
        }
        Ok(())
    }

    pub(crate) fn shared_history_enabled(&self, id: &str) -> Option<bool> {
        let action = HistoryAction::from_command(id)?;
        let room = self.collaboration.room.as_ref()?;
        self.session.active().filter(|st| st.uid == room.doc_uid)?;
        Some(
            room.can_edit()
                && !self.session.has_deferred_edit()
                && room.local.as_ref().is_some_and(|peer| room.history_patch(peer.actor, action).is_ok()),
        )
    }

    pub(crate) fn shared_history_command(&mut self, id: &str) -> Option<Result<(), String>> {
        let action = HistoryAction::from_command(id)?;
        let room = self.collaboration.room.as_mut()?;
        if !self.session.active().is_some_and(|st| st.uid == room.doc_uid) {
            return None;
        }
        if !room.can_edit() || self.session.has_deferred_edit() {
            return Some(Err("Wait for the shared document to catch up first.".into()));
        }
        Some(room.queue_proposal(&mut self.session, Vec::new(), Some(action)))
    }

    pub(crate) fn open_collaboration_dialog(&mut self) {
        self.collaboration.open = true;
    }

    fn start_seed_download(&mut self) -> Result<(), String> {
        let c = &mut self.collaboration;
        c.joining = Some(PendingJoin::start(&c.join_code, &c.name)?);
        c.join_code.clear();
        c.message = None;
        c.room_conflict = None;
        Ok(())
    }

    fn poll_seed_download(&mut self, ctx: &egui::Context) {
        let Some(joining) = self.collaboration.joining.as_mut() else { return };
        match joining.poll() {
            Ok(Some(doc)) => {
                if let Some(joining) = self.collaboration.joining.take()
                    && let Err(error) = self.install_downloaded_seed(joining, doc)
                {
                    self.collaboration.message = Some(error);
                }
            }
            Ok(None) => ctx.request_repaint_after(Duration::from_millis(40)),
            Err(error) => {
                self.collaboration.joining = None;
                self.collaboration.message = Some(error);
            }
        }
    }

    fn install_downloaded_seed(&mut self, joining: PendingJoin, seed_download: JoinedSeed) -> Result<(), String> {
        let JoinedSeed { document: doc, originals } = seed_download;
        let protocol = joining.protocol;
        let seed = seed_document::Fingerprints::for_room(protocol).canonical(&doc)?;
        let identity = joining.identity.to_bytes();
        let storage = if originals.is_pending() {
            Storage::ephemeral_preview_for_protocol(doc.clone(), identity, protocol)
        } else {
            Storage::ephemeral_for_protocol(doc.clone(), identity, protocol)?
        };
        let state = designcraft_engine::DocState::new(doc, None);
        let uid = state.uid;
        self.session.add_document(state);
        let mut room = Room::new(joining.transport, Mode::Guest { ready: false, hello_sent: false }, storage, uid, seed);
        room.local = joining.local;
        room.host = Some(joining.host);
        room.online = true;
        room.peers = joining.peers;
        room.originals = originals;
        self.collaboration.room = Some(room);
        self.session.defer_document_edits(Some(uid));
        // An invitation download has no local file. Keep it off disk until the user saves.
        self.session.set_deferred_edit_durable(true);
        self.after_engine();
        Ok(())
    }

    fn reconnect_collaboration(&mut self) -> Result<(), String> {
        let c = &mut self.collaboration;
        let room = c.room.as_mut().ok_or("Open a shared room before reconnecting.")?;
        if !matches!(room.mode, Mode::Guest { .. }) {
            return Err("Only a guest can reconnect through a new invitation.".into());
        }
        let (protocol, ticket) = compatible_invitation(&c.join_code)?;
        let host = Transport::invitation_host(ticket)?;
        if room.host != Some(host) || room.protocol != protocol {
            return Err("Use a current invitation from the same room host.".into());
        }
        let connection = Transport::join_with_identity(ticket, &c.name, room.storage.identity())?;
        room.transport = connection;
        room.mode = Mode::Guest { ready: false, hello_sent: false };
        room.online = false;
        room.peers.clear();
        room.presence.clear();
        if !room.story_queue_blocked {
            room.error = None;
        }
        room.fatal = false;
        room.last_send = None;
        room.receiving_edits.clear();
        room.receiving_blobs.clear();
        room.originals.disconnect();
        room.sending_proposal = None;
        room.story_sender = Default::default();
        c.join_code.clear();
        Ok(())
    }

    fn connect_collaboration(&mut self, host: bool) -> Result<(), String> {
        if host {
            self.collaboration.room_conflict = None;
        }
        self.connect_to_room(host, OpenRoom::Active, None)
    }

    fn start_new_room_from_current(&mut self) -> Result<(), String> {
        let conflict = self
            .collaboration
            .room_conflict
            .take()
            .filter(|conflict| conflict.applies(&self.session))
            .ok_or("Select Start sharing again to check this document version.")?;
        self.connect_to_room(true, OpenRoom::NewHost(conflict.previous), Some(conflict.protocol))
    }

    fn open_saved_room_copy(&mut self) -> Result<(), String> {
        let st = self.session.active().ok_or("Open the saved document before recovering a room.")?;
        let conflict = self.collaboration.room_conflict.as_ref().filter(|conflict| conflict.applies(&self.session));
        let (storage, saved) = Storage::open_for_protocol(
            self.session.recovery_dir.as_deref(),
            st.path.as_deref(),
            &st.doc,
            None,
            OpenRoom::SavedHost(conflict.map(|c| c.previous)),
            conflict.map(|c| c.protocol),
        )?;
        let recovered = replay::restore_for_room(&storage.seed, &storage.seed, saved.confirmed, storage.protocol)?;
        self.session.add_document(designcraft_engine::DocState::new(recovered.document, None));
        self.collaboration.open = false;
        self.collaboration.room_conflict = None;
        self.collaboration.message = None;
        self.after_engine();
        Ok(())
    }

    fn connect_to_room(&mut self, host: bool, room: OpenRoom, preferred_protocol: Option<RoomProtocol>) -> Result<(), String> {
        let st = self.session.active().ok_or("Open the saved document before connecting.")?;
        if st.interaction.is_some() || st.preview_stash.is_some() {
            return Err("Finish the current document edit before connecting.".into());
        }
        let c = &mut self.collaboration;
        let invitation = (!host).then(|| compatible_invitation(&c.join_code)).transpose()?;
        let guest_of = invitation.map(|(_, ticket)| Transport::invitation_host(ticket)).transpose()?;
        if cfg!(target_os = "macos") && self.session.recovery_dir.is_some() && (st.path.is_none() || st.is_dirty()) {
            return Err("Save the seed document before starting or joining a room.".into());
        }
        let mut opened = (*st.doc).clone();
        seed_document::prepare_local_copy(&mut opened)?;
        let requested_protocol = invitation.map(|(p, _)| p).or(preferred_protocol);
        let (storage, saved) =
            Storage::open_for_protocol(self.session.recovery_dir.as_deref(), st.path.as_deref(), &opened, guest_of, room, requested_protocol)?;
        let protocol = storage.protocol;
        let mut fingerprints = seed_document::Fingerprints::for_room(protocol);
        let seed = fingerprints.canonical(&storage.seed)?;
        let uid = st.uid;
        let recovered = match replay::restore_cached_for_room(&storage.seed, &opened, saved.confirmed, &mut fingerprints, protocol) {
            Ok(recovered) => recovered,
            Err(issue) => {
                if host
                    && matches!(room, OpenRoom::Active)
                    && matches!(issue, replay::RestoreIssue::Diverged(_))
                    && let Some(previous) = storage.host_room_id()
                    && let Some(path) = st.path.clone()
                {
                    c.room_conflict = Some(RoomConflict { doc_uid: uid, revision: st.revision, path, previous, protocol, focus_start: true });
                }
                return Err(issue.into_message());
            }
        };
        let revision = recovered.history.len() as u64;
        if saved.pending.iter().any(|pending| {
            host || pending.base > revision
                || pending.sequence >= saved.next_sequence
                || pending.validate_story_transaction_for_protocol(protocol).is_err()
        }) {
            return Err("The local collaboration journal has an invalid pending edit.".into());
        }
        let (transport, mode) = if let Some((_, ticket)) = invitation {
            (Transport::join_with_identity(ticket, &c.name, storage.identity())?, Mode::Guest { ready: false, hello_sent: false })
        } else {
            (Transport::host_with_identity(&c.name, c.relay, storage.identity())?, Mode::Host { delivery: BTreeMap::new() })
        };
        if !matches!(room, OpenRoom::NewHost(_)) {
            storage.write(recovered.history.iter().map(|(commit, _)| commit), &saved.pending, saved.next_sequence)?;
        }
        let durable = storage.is_durable();
        if let Some(st) = self.session.active_mut() {
            install_confirmed(st, recovered.document);
        }
        c.join_code.clear();
        let mut room = Room::new(transport, mode, storage, uid, seed);
        room.restore_history_index(recovered.history);
        room.inverses = recovered.inverses;
        room.history_bytes = recovered.history_bytes;
        room.replica = recovered.replica;
        room.host = guest_of;
        room.next_sequence = saved.next_sequence;
        room.revision = revision;
        room.pending = saved.pending;
        if protocol.is_automerge() && !host && durable && (revision != 0 || !room.pending.is_empty()) {
            room.has_joined = true;
            room.local = Some(Peer { actor: room.storage.identity().actor(), name: c.name.clone() });
        }
        room.restore_story_draft(&mut self.session);
        self.session.defer_document_edits(Some(uid));
        self.session.set_deferred_edit_durable(durable);
        room.update_edit_policy(&mut self.session);
        c.room = Some(room);
        c.message = None;
        c.room_conflict = None;
        Ok(())
    }
}

fn compatible_invitation(code: &str) -> Result<(RoomProtocol, &str), String> {
    let invitation = RoomProtocol::parse_invitation(code);
    if !invitation.0.is_supported() {
        return Err("Update Collate to join this room.".into());
    }
    Ok(invitation)
}

impl Room {
    fn new(transport: Transport, mode: Mode, storage: Storage, doc_uid: u64, seed: [u8; 32]) -> Self {
        let seed_artwork = seed_document::artwork_size(&storage.seed);
        let protocol = storage.protocol;
        let replica = Replica::for_room(&storage.seed, protocol);
        Self {
            transport,
            #[cfg(unix)]
            server_journal: None,
            mode,
            protocol,
            storage,
            doc_uid,
            seed,
            replica,
            history: Vec::new(),
            inverses: Vec::new(),
            history_bytes: 0,
            committed_sequences: BTreeMap::new(),
            history_artwork: 0,
            seed_artwork,
            model_size: None,
            seed_delivery: SeedDelivery::default(),
            originals: OriginalDownloads::default(),
            receiving_edits: BTreeMap::new(),
            receiving_blobs: BTreeMap::new(),
            sending_proposal: None,
            online: false,
            has_joined: false,
            local: None,
            host: None,
            peers: BTreeMap::new(),
            presence: BTreeMap::new(),
            invitation: None,
            next_sequence: 1,
            revision: 0,
            pending: VecDeque::new(),
            pending_story_selection: None,
            story_draft: None,
            story_queue_blocked: false,
            story_sender: Default::default(),
            rejected_comment: None,
            frame_entry: None,
            layer_entry: None,
            typing: None,
            last_send: None,
            last_presence: None,
            error: None,
            fatal: false,
            #[cfg(target_os = "macos")]
            recovery_path_checked: None,
        }
    }

    fn restore_history_index(&mut self, history: Vec<(Commit, WireEdit)>) {
        self.committed_sequences = history.iter().enumerate().map(|(i, (commit, _))| ((commit.actor, commit.proposal.sequence), i)).collect();
        self.history_artwork = history.iter().map(|(_, wire)| wire.artwork_size).sum();
        self.history = history;
    }

    #[cfg(target_os = "macos")]
    fn enable_recovery(&mut self, recovery_dir: Option<&std::path::Path>, path: &str) {
        if self.storage.is_durable() || self.originals.is_pending() || recovery_dir.is_none() || self.recovery_path_checked.as_deref() == Some(path) {
            return;
        }
        let Mode::Guest { .. } = self.mode else { return };
        let Some(host) = self.host else { return };
        self.recovery_path_checked = Some(path.into());
        if let Err(error) =
            self.storage.enable_journal(recovery_dir, path, host, self.history.iter().map(|(c, _)| c), &self.pending, self.next_sequence)
        {
            self.error = Some(error);
        }
    }

    fn can_edit(&self) -> bool {
        !self.fatal
            && self.local.is_some()
            && self.pending.is_empty()
            && match self.mode {
                Mode::Host { .. } => {
                    self.online && self.history.len() < self.protocol.history_limit() && self.history_bytes < self.protocol.history_byte_limit()
                }
                Mode::Guest { ready, .. } => ready || self.has_joined && !self.online && self.storage.is_durable(),
            }
    }

    fn can_draft_story(&self) -> bool {
        self.protocol.is_automerge()
            && !self.fatal
            && !self.story_queue_blocked
            && self.local.is_some()
            && self.pending.len() < MAX_QUEUED_STORY_EDITS
            && self.pending.front().is_none_or(|pending| pending.story_transaction.is_some())
            && matches!(self.mode, Mode::Guest { ready, .. } if ready || self.has_joined && !self.online && self.storage.is_durable())
    }

    fn update_edit_policy(&self, session: &mut Session) {
        session.set_deferred_edit_policy(self.can_edit(), self.can_draft_story(), self.story_draft.is_some());
    }

    fn restore_story_draft(&mut self, session: &mut Session) {
        let Some(st) = session.document_by_uid_mut(self.doc_uid) else { return };
        match story_sync::rebase(&st.doc, &self.replica, self.storage.identity().actor(), &self.pending) {
            Ok((document, draft)) => {
                install_document(st, document);
                self.story_draft = draft;
            }
            Err(_) => self.block_story_queue(),
        }
    }

    fn poll(&mut self, session: &mut Session) {
        if let Some(edit) = session.take_deferred_edit()
            && let Err(error) = self.capture_edit(session, edit)
        {
            self.pause_typing();
            self.error = Some(error);
        }
        for _ in 0..32 {
            let Some(event) = self.transport.try_event() else { break };
            self.on_event(session, event);
        }
        let expired: Vec<_> = self.receiving_blobs.iter().filter(|(_, stream)| stream.expired()).map(|(&peer, _)| peer).collect();
        for peer in expired {
            self.drop_transfer(peer);
            if matches!(self.mode, Mode::Host { .. }) {
                self.reply(peer, Message::RetryArtwork);
            }
        }
        self.advance_delivery();
        self.seed_delivery.advance(&self.transport, &self.storage.seed_blobs, self.protocol);
        self.advance_proposal();
        self.advance_original_delivery();
    }

    fn artwork_progress(&self) -> Option<attachments::ArtworkProgress> {
        if let Some(progress) = self.receiving_blobs.values().find_map(|stream| stream.progress()) {
            return Some(progress);
        }
        match &self.mode {
            Mode::Guest { .. } => {
                self.sending_proposal.as_ref().and_then(|(wire, cursor)| wire.progress(cursor)).or_else(|| self.originals.progress())
            }
            Mode::Host { delivery } => self
                .seed_delivery
                .progress(&self.storage.seed_blobs)
                .or_else(|| delivery.values().find_map(|d| self.history.get(d.sent)?.1.progress(&d.cursor))),
        }
    }

    fn advance_proposal(&mut self) {
        if self.story_queue_blocked {
            return;
        }
        let (Mode::Guest { ready: true, .. }, Some(pending)) = (&self.mode, self.pending.front()) else { return };
        if pending.story_transaction.is_some() {
            if let Err(error) = self.story_sender.advance(&self.pending, |frame| self.transport.submit(frame)) {
                self.stop_with_error(&error);
            }
            return;
        }
        if self.sending_proposal.is_none() && self.last_send.is_some() && self.host.is_some_and(|host| self.receiving_blobs.contains_key(&host)) {
            return;
        }
        if self.sending_proposal.is_none() && self.last_send.is_none_or(|last| last.elapsed() >= Duration::from_secs(2)) {
            self.sending_proposal = match encode_edit_for_protocol(Message::Propose(pending.clone()), self.protocol) {
                Ok(mut wire) => {
                    // The ordered transport delivered the attachments before the
                    // proposal. Retry just its metadata while waiting for a receipt.
                    if self.last_send.is_some() {
                        wire.blobs.clear();
                    }
                    Some((wire, SendCursor::default()))
                }
                Err(error) => return self.stop_with_error(&error),
            };
        }
        let Some((wire, cursor)) = self.sending_proposal.as_mut() else { return };
        for _ in 0..32 {
            match wire.send_next(cursor, |frame| self.transport.submit(frame)) {
                Ok(true) => {
                    self.sending_proposal = None;
                    self.last_send = Some(Instant::now());
                    break;
                }
                Ok(false) => {}
                Err(_) => break,
            }
        }
    }

    fn on_event(&mut self, session: &mut Session, event: Event) {
        match event {
            Event::Ready { local, host } if !self.fatal => {
                if let Err(error) = self.storage.activate() {
                    self.stop_with_error(&error);
                    return;
                }
                self.local = Some(local);
                self.online = true;
                self.host = Some(host);
                self.last_send = None;
            }
            Event::Ready { .. } => {}
            Event::Invitation(code) if !self.fatal => self.invitation = Some(self.protocol.decorate_invitation(code)),
            Event::Invitation(_) => {}
            Event::PeerJoined(peer) => {
                self.peers.insert(peer.actor, peer);
            }
            Event::PeerLeft(actor) => {
                self.peers.remove(&actor);
                self.presence.remove(&actor);
                self.seed_delivery.stop(actor);
                self.receiving_blobs.remove(&actor);
                self.receiving_edits.remove(&actor);
                if let Mode::Host { delivery, .. } = &mut self.mode {
                    delivery.remove(&actor);
                }
            }
            Event::Message { actor, bytes } => self.on_packet(session, actor, &bytes),
            Event::Disconnected => self.disconnected(),
            Event::Failed(error)
                if self.protocol.is_automerge() && matches!(self.mode, Mode::Guest { .. }) && self.has_joined && self.storage.is_durable() =>
            {
                self.disconnected();
                if !self.story_queue_blocked {
                    self.error = Some(error);
                }
            }
            Event::Failed(error) => self.stop_with_error(&error),
        }
    }

    fn disconnected(&mut self) {
        self.online = false;
        self.peers.clear();
        self.presence.clear();
        self.receiving_edits.clear();
        self.receiving_blobs.clear();
        self.originals.disconnect();
        self.sending_proposal = None;
        self.story_sender = Default::default();
        if matches!(self.mode, Mode::Guest { .. }) {
            self.mode = Mode::Guest { ready: false, hello_sent: false };
        }
    }

    fn on_packet(&mut self, session: &mut Session, actor: ActorId, bytes: &[u8]) {
        if self.fatal {
            return;
        }
        if self.on_original_packet(actor, bytes) {
            return;
        }
        if attachments::is_part(bytes) {
            if self.can_receive_edit(actor) {
                let other: usize = self.receiving_blobs.iter().filter(|(a, _)| **a != actor).map(|(_, b)| b.reserved()).sum();
                let allowance = designcraft_collab::artwork::MAX_ARTWORK_BYTES.saturating_sub(other);
                if let Err(error) = self.receiving_blobs.entry(actor).or_default().append(bytes, allowance) {
                    self.drop_transfer(actor);
                    self.error = Some(error);
                }
            }
            return;
        }
        let Ok(packet) = serde_json::from_slice::<Packet>(bytes) else { return };
        if packet.version != self.protocol.version() {
            if matches!(self.mode, Mode::Host { .. }) {
                self.reply(actor, Message::Incompatible);
            } else {
                self.stop_with_error("This room uses a different collaboration protocol.");
            }
            return;
        }
        match packet.message {
            Message::EditOffer(offer) if self.can_receive_edit(actor) => match edit_wire::start(offer) {
                Ok(download) => {
                    self.receiving_edits.insert(actor, download);
                }
                Err(_) => self.drop_transfer(actor),
            },
            Message::EditPart(part) if self.can_receive_edit(actor) => self.receive_edit_part(session, actor, part),
            message if matches!(self.mode, Mode::Host { .. }) => self.on_guest_message(session, actor, message),
            message if self.host == Some(actor) => self.on_host_message(session, message),
            _ => {}
        }
    }

    fn can_receive_edit(&self, actor: ActorId) -> bool {
        match self.mode {
            Mode::Host { .. } => self.serving(actor),
            Mode::Guest { hello_sent, .. } => self.online && hello_sent && self.host == Some(actor),
        }
    }

    fn receive_edit_part(&mut self, session: &mut Session, actor: ActorId, part: SeedChunk) {
        let Some(download) = self.receiving_edits.get_mut(&actor) else { return };
        let bytes = match download.append(part) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return,
            Err(_) => return self.drop_transfer(actor),
        };
        self.receiving_edits.remove(&actor);
        let Ok(Packet { version, message }) = serde_json::from_slice(&bytes) else { return self.drop_transfer(actor) };
        if version != self.protocol.version() {
            return self.drop_transfer(actor);
        }
        match message {
            Message::Propose(ref proposal) if self.serving(actor) && accepts_chunked_edit(proposal) => {
                self.on_guest_message(session, actor, message);
            }
            Message::Commit(ref commit) if self.host == Some(actor) && accepts_chunked_edit(&commit.proposal) => {
                self.on_host_message(session, message);
            }
            _ => self.drop_transfer(actor),
        }
    }

    fn drop_transfer(&mut self, actor: ActorId) {
        self.receiving_edits.remove(&actor);
        self.receiving_blobs.remove(&actor);
        if matches!(self.mode, Mode::Guest { .. }) {
            self.mode = Mode::Guest { ready: false, hello_sent: false };
            self.error = Some("Requesting missing edits from the host.".into());
        }
    }

    fn on_guest_message(&mut self, session: &mut Session, actor: ActorId, message: Message) {
        match message {
            Message::RequestSeed => self.provide_seed(actor),
            Message::RequestOriginal { blob } if self.protocol.is_automerge() && self.serving(actor) => self.provide_original(actor, blob),
            Message::Hello { seed, revision } => {
                if seed != self.seed || revision > self.revision {
                    self.reply(actor, Message::Incompatible);
                } else if let Mode::Host { delivery, .. } = &mut self.mode {
                    self.receiving_edits.remove(&actor);
                    self.receiving_blobs.remove(&actor);
                    delivery.insert(actor, Delivery { sent: revision as usize, cursor: SendCursor::default(), announced: false, receipt: None });
                }
            }
            Message::Propose(mut proposal) if self.serving(actor) => {
                let sequence = proposal.sequence;
                let transfer = self.receiving_blobs.remove(&actor).unwrap_or_default();
                let duplicate = self.committed_sequences.contains_key(&(actor, sequence));
                let uploaded = !duplicate && transfer.reserved() > 0 && proposal.history_action.is_none();
                let validated = if duplicate {
                    Ok(())
                } else {
                    self.check_artwork_budget(proposal.artwork_size()).and_then(|()| transfer.attach(&mut proposal, None))
                };
                if let Err(error) = validated.and_then(|()| self.admit(session, actor, proposal)) {
                    self.reply(actor, Message::Declined { sequence, artwork_limit: error == attachments::ARTWORK_LIMIT });
                } else if uploaded
                    && let Mode::Host { delivery } = &mut self.mode
                    && let Some(state) = delivery.get_mut(&actor)
                {
                    state.receipt = self.history.len().checked_sub(1);
                }
            }
            Message::Presence { state, .. } if self.serving(actor) && state.is_valid() => {
                self.presence.insert(actor, state.clone());
                self.relay_presence(actor, state);
            }
            _ => {}
        }
    }

    fn on_host_message(&mut self, session: &mut Session, message: Message) {
        match message {
            Message::ArtworkReceipt { revision, sequence, digest } => {
                if revision > self.revision
                    && let Err(error) = self.confirm_upload(session, revision, sequence, digest)
                {
                    self.stop_with_error(&error);
                }
            }
            Message::Commit(mut commit) => {
                if let Some(host) = self.host
                    && let Err(error) = self.receiving_blobs.remove(&host).unwrap_or_default().attach(&mut commit.proposal, self.pending.front())
                {
                    return self.stop_with_error(&error);
                }
                if commit.revision > self.revision {
                    if commit.revision != self.revision + 1 {
                        self.mode = Mode::Guest { ready: false, hello_sent: false };
                        self.error = Some("Requesting missing edits from the host.".into());
                        return;
                    }
                    if let Err(error) = self.commit(session, commit) {
                        self.stop_with_error(&error);
                    }
                } else if self.matches_pending(&commit)
                    && let Err(error) = self.acknowledge_duplicate(session)
                {
                    self.stop_with_error(&error);
                }
            }
            Message::CaughtUp { revision } if revision == self.revision => {
                self.has_joined = true;
                if !self.story_queue_blocked {
                    self.error = None;
                }
                self.mode = Mode::Guest { ready: true, hello_sent: true };
            }
            Message::Declined { sequence, artwork_limit } => self.decline_pending(session, sequence, artwork_limit),
            Message::RetryArtwork if !self.pending.is_empty() => {
                self.sending_proposal = None;
                self.last_send = None;
            }
            Message::Presence { actor, state } if state.is_valid() && self.peers.contains_key(&actor) => {
                self.presence.insert(actor, state);
            }
            Message::Incompatible => self.stop_with_error("Open the same saved document as the host and try again."),
            _ => {}
        }
    }

    fn confirm_upload(&mut self, session: &mut Session, revision: u64, sequence: u64, digest: [u8; 32]) -> Result<(), String> {
        if revision == self.revision + 1
            && let Some(local) = &self.local
            && let Some(pending) = self.pending.front().filter(|p| p.sequence == sequence)
        {
            let candidate = Commit { revision, actor: local.actor, proposal: pending.clone(), correction: None };
            if encode_edit_for_protocol(Message::Commit(candidate.clone()), self.protocol).is_ok_and(|wire| wire.digest() == digest) {
                return self.commit(session, candidate);
            }
        }
        // A restarted or out-of-date guest may no longer own this exact pending
        // proposal. A fresh Hello requests the complete commit and verified body.
        self.mode = Mode::Guest { ready: false, hello_sent: false };
        self.error = Some("Requesting missing edits from the host.".into());
        Ok(())
    }

    fn stop_with_error(&mut self, error: &str) {
        self.error = Some(error.into());
        self.fatal = true;
    }

    fn serving(&self, actor: ActorId) -> bool {
        matches!(&self.mode, Mode::Host { delivery, .. } if delivery.get(&actor).is_some_and(|d| d.announced))
    }

    fn reply(&self, actor: ActorId, message: Message) {
        if let Ok(bytes) = encode_for_protocol(message, self.protocol) {
            let _ = self.transport.send_to(actor, bytes);
        }
    }

    fn advance_delivery(&mut self) {
        match &mut self.mode {
            Mode::Host { delivery } => {
                for (&actor, state) in delivery {
                    for _ in 0..32 {
                        let Some((commit, wire)) = self.history.get(state.sent) else { break };
                        let sent = if state.receipt == Some(state.sent) {
                            let receipt =
                                Message::ArtworkReceipt { revision: commit.revision, sequence: commit.proposal.sequence, digest: wire.digest() };
                            encode_for_protocol(receipt, self.protocol).and_then(|frame| self.transport.send_to(actor, frame)).map(|_| true)
                        } else {
                            wire.send_next(&mut state.cursor, |frame| self.transport.send_to(actor, frame))
                        };
                        match sent {
                            Ok(true) => {
                                if state.receipt == Some(state.sent) {
                                    state.receipt = None;
                                }
                                state.sent += 1;
                                state.cursor = SendCursor::default();
                            }
                            Ok(false) => {}
                            Err(_) => break,
                        }
                    }
                    if state.sent == self.history.len()
                        && !state.announced
                        && let Ok(bytes) = encode_for_protocol(Message::CaughtUp { revision: self.revision }, self.protocol)
                        && self.transport.send_to(actor, bytes).is_ok()
                    {
                        state.announced = true;
                    }
                }
            }
            Mode::Guest { hello_sent, .. } => {
                if !*hello_sent
                    && self.online
                    && let Ok(bytes) = encode_for_protocol(Message::Hello { seed: self.seed, revision: self.revision }, self.protocol)
                    && self.transport.submit(bytes).is_ok()
                {
                    *hello_sent = true;
                }
            }
        }
    }

    fn capture_edit(&mut self, session: &mut Session, edit: DeferredEdit) -> Result<(), String> {
        let actor = self.local.as_ref().ok_or("The collaboration connection is still starting.")?.actor;
        self.error = None;
        let local = self.story_draft.as_ref().map_or(&self.replica, |draft| &draft.visible);
        if self.protocol.is_automerge()
            && let Some(story_sync::Captured { transaction, selection, replica }) = story_sync::capture(local, actor, &edit, self.next_sequence)?
        {
            if matches!(self.mode, Mode::Guest { .. }) {
                if !self.can_draft_story() {
                    return Err("Wait for your story edits to sync before typing more.".into());
                }
                self.queue_transaction(session, Vec::new(), None, Some(transaction))?;
                let confirmed = self.story_draft.as_ref().map_or(edit.before, |draft| draft.confirmed.clone());
                self.story_draft = Some(story_sync::Draft { confirmed, visible: replica });
                let st = session.document_by_uid_mut(self.doc_uid).ok_or("The shared document closed.")?;
                install_document(st, edit.after);
                st.selection = edit.after_selection;
                return Ok(());
            }
            self.pending_story_selection = selection;
            return self.queue_transaction(session, Vec::new(), None, Some(transaction)).inspect_err(|_| self.pending_story_selection = None);
        }
        if !self.pending.is_empty() {
            return Err("Wait for your story edits to sync before changing the layout.".into());
        }
        let selections = edit.before_selection.text.zip(edit.after_selection.text);
        let edits = self
            .replica
            .capture_selected(&edit.before, &edit.after, &actor.to_string(), self.next_sequence, selections)
            .map_err(|error| error.to_string())?;
        self.frame_entry = typing::FrameEntry::capture(self.next_sequence, &edit, &edits);
        self.layer_entry =
            matches!(edits.as_slice(), [Operation::Layer { edit: LayerEdit::Create { .. } }]).then_some((self.next_sequence, edit.before_layer));
        let result = self.queue_local(session, edits);
        if result.is_err() {
            self.frame_entry = None;
            self.layer_entry = None;
        }
        result
    }

    fn queue_local(&mut self, session: &mut Session, edits: Vec<Operation>) -> Result<(), String> {
        if edits.is_empty() {
            return Ok(());
        }
        self.queue_proposal(session, edits, None)
    }

    fn queue_proposal(&mut self, session: &mut Session, edits: Vec<Operation>, history_action: Option<HistoryAction>) -> Result<(), String> {
        self.queue_transaction(session, edits, history_action, None)
    }

    fn queue_transaction(
        &mut self,
        session: &mut Session,
        edits: Vec<Operation>,
        history_action: Option<HistoryAction>,
        story_transaction: Option<StoryTransaction>,
    ) -> Result<(), String> {
        let actor = self.local.as_ref().ok_or("The collaboration connection is still starting.")?.actor;
        let proposal = Proposal { sequence: self.next_sequence, base: self.revision, edits, history_action, story_transaction };
        let next_sequence = self.next_sequence.checked_add(1).ok_or("The shared room reached its edit limit.")?;
        let wire = encode_edit_for_protocol(Message::Propose(proposal.clone()), self.protocol)?;
        self.check_artwork_budget(wire.artwork_size)?;
        if matches!(self.mode, Mode::Host { .. }) {
            self.next_sequence = next_sequence;
            self.admit(session, actor, proposal)
        } else if self.pending.is_empty() || proposal.story_transaction.is_some() && self.can_draft_story() {
            let is_story = proposal.story_transaction.is_some();
            self.pending.push_back(proposal);
            let saved = if is_story {
                self.storage.write_story_outbox(self.revision, &self.pending, next_sequence)
            } else {
                self.storage.write(self.history.iter().map(|(c, _)| c), &self.pending, next_sequence)
            };
            if let Err(error) = saved {
                self.pending.pop_back();
                return Err(error);
            }
            self.next_sequence = next_sequence;
            if self.pending.len() == 1 && !is_story {
                self.sending_proposal = Some((wire, SendCursor::default()));
                self.last_send = None;
            }
            Ok(())
        } else {
            Err("Wait for the host to confirm your previous edit.".into())
        }
    }

    fn admit(&mut self, session: &mut Session, actor: ActorId, mut proposal: Proposal) -> Result<(), String> {
        if !matches!(self.mode, Mode::Host { .. }) {
            return Err("Only the host can confirm edits.".into());
        }
        if let Some(&index) = self.committed_sequences.get(&(actor, proposal.sequence)) {
            if let Mode::Host { delivery } = &mut self.mode
                && let Some(state) = delivery.get_mut(&actor).filter(|state| state.sent > index)
            {
                state.sent = index;
                state.cursor = SendCursor::default();
            }
            return Ok(());
        }
        if proposal.base > self.revision || proposal.sequence == 0 || self.history.len() >= self.protocol.history_limit() {
            return Err("The edit has an invalid revision or the room is full.".into());
        }
        proposal.validate_story_transaction_for_protocol(self.protocol)?;
        if proposal.edits.iter().any(Operation::is_text_compensation) {
            return Err("A text undo or redo must be requested through shared history.".into());
        }
        let correction = if let Some((original, inverse)) = self.resolve_history(actor, &proposal)? {
            proposal.edits = inverse;
            proposal.base = self.revision;
            Some(original)
        } else {
            None
        };
        if proposal.edits.is_empty() && proposal.story_transaction.is_none() {
            return Err("The proposal has no edits.".into());
        }
        let author = actor.to_string();
        if !proposal.edits.iter().all(|edit| edit.is_authored_by(&author) && edit.is_from_sequence(proposal.sequence)) {
            return Err("Could not verify the edit author.".into());
        }
        if self.history.iter().skip(proposal.base as usize).any(|(old, _)| proposal.conflicts_with(&old.proposal)) {
            return Err("A collaborator changed the same object property first.".into());
        }
        self.commit(session, Commit { revision: self.revision + 1, actor, proposal, correction })
    }

    fn matches_pending(&self, commit: &Commit) -> bool {
        self.local.as_ref().is_some_and(|local| commit.actor == local.actor)
            && self.pending.front().is_some_and(|p| p.sequence == commit.proposal.sequence)
    }

    fn clear_pending(&mut self, session: &mut Session) -> Result<(), String> {
        if !self.pending.is_empty() {
            self.storage.write(self.history.iter().map(|(c, _)| c), &VecDeque::new(), self.next_sequence)?;
            self.pending.clear();
            self.sending_proposal = None;
            self.last_send = None;
            self.pending_story_selection = None;
            self.story_queue_blocked = false;
            self.story_sender = Default::default();
            if let Some(draft) = self.story_draft.take()
                && let Some(st) = session.document_by_uid_mut(self.doc_uid)
            {
                let selection = story_sync::remap_selection(st, &draft.visible, &self.replica);
                install_document(st, draft.confirmed);
                st.selection.text = selection;
            }
            self.layer_entry = None;
            if self.frame_entry.take().is_some() {
                self.pause_typing();
            }
        }
        Ok(())
    }

    fn acknowledge_duplicate(&mut self, session: &mut Session) -> Result<(), String> {
        let Some(front) = self.pending.pop_front() else { return Ok(()) };
        if let Err(error) = self.storage.write(self.history.iter().map(|(c, _)| c), &self.pending, self.next_sequence) {
            self.pending.push_front(front);
            return Err(error);
        }
        self.sending_proposal = None;
        self.last_send = None;
        self.story_sender.acknowledge(&self.pending);
        if self.pending.is_empty() {
            self.story_queue_blocked = false;
        }
        if let Some(draft) = self.story_draft.take()
            && let Some(st) = session.document_by_uid_mut(self.doc_uid)
        {
            let rebased = story_sync::rebase(&draft.confirmed, &self.replica, self.storage.identity().actor(), &self.pending);
            self.install_rebased(st, rebased, draft.confirmed.clone(), Some(draft), &self.replica.clone());
        }
        Ok(())
    }

    fn block_story_queue(&mut self) {
        self.story_queue_blocked = true;
        self.sending_proposal = None;
        self.story_sender = Default::default();
        self.pause_typing();
        self.error = Some("Some story edits could not sync. Copy your queued typing or save a copy of the open draft before leaving.".into());
    }

    fn declined_story_text(&self) -> String {
        if self.story_queue_blocked { self.pending.iter().map(typing::submitted_typing).collect() } else { String::new() }
    }

    fn decline_pending(&mut self, session: &mut Session, sequence: u64, artwork_limit: bool) {
        let Some(proposal) = self.pending.front().filter(|proposal| proposal.sequence == sequence) else { return };
        if proposal.story_transaction.is_some() {
            // Later changes can depend on this one. Retain the entire outbox and the visible draft.
            self.block_story_queue();
            return;
        }
        let submitted = typing::submitted_typing(proposal);
        let comment = match proposal.edits.as_slice() {
            [Operation::CreateComment { anchor, text, .. }] => {
                Some(CommentDraft { text: text.clone(), reply_to: None, needs_text_selection: anchor.is_text() })
            }
            [Operation::Reply { thread, text, .. }] => {
                Some(CommentDraft { text: text.clone(), reply_to: Some(thread.clone()), needs_text_selection: false })
            }
            _ => None,
        };
        if let Err(error) = self.clear_pending(session) {
            self.stop_with_error(&error);
            return;
        }
        self.rejected_comment = comment;
        self.error = Some(
            if artwork_limit { attachments::ARTWORK_LIMIT } else { "The host could not accept this edit. The confirmed document is still open." }
                .into(),
        );
        self.preserve_rejected_typing(submitted);
    }

    fn check_artwork_budget(&self, next: u64) -> Result<(), String> {
        if self.seed_artwork + self.history_artwork + next > designcraft_collab::artwork::MAX_ARTWORK_BYTES as u64 {
            return Err(attachments::ARTWORK_LIMIT.into());
        }
        Ok(())
    }

    fn commit(&mut self, session: &mut Session, commit: Commit) -> Result<(), String> {
        if commit.proposal.sequence == 0 || self.committed_sequences.contains_key(&(commit.actor, commit.proposal.sequence)) {
            return Err("The confirmed edit has an invalid sequence.".into());
        }
        history::verify_text_compensation(&commit, self.history.iter().map(|(original, _)| original), &self.inverses)?;
        let bytes = encode_edit_for_protocol(Message::Commit(commit.clone()), self.protocol)?;
        self.check_artwork_budget(bytes.artwork_size)?;
        if self.history.len() >= self.protocol.history_limit() || self.history_bytes.saturating_add(bytes.len()) > self.protocol.history_byte_limit()
        {
            return Err("The shared room reached its history limit.".into());
        }
        let st = session.document_by_uid_mut(self.doc_uid).ok_or("The shared document closed.")?;
        let base = self.story_draft.as_ref().map_or_else(|| st.doc.clone(), |draft| draft.confirmed.clone());
        let mut doc = (*base).clone();
        let mut replica = self.replica.clone();
        let inverse = commit.apply_document(&mut replica, &mut doc)?;
        if commit.proposal.edits.iter().any(Operation::changes_artwork) {
            seed_document::check_artwork_for_room(&doc, self.protocol)?;
        }
        // Every committed checkpoint must fit the model bound during journal replay.
        let model_size = match &self.model_size {
            Some((before, size)) if before.ptr_eq(&Arc::downgrade(&base)) && commit.proposal.is_text_only() => {
                seed_document::updated_story_model_size(*size, &base, &doc, commit.proposal.affected_stories())?
            }
            _ => seed_document::model_size(&doc)?,
        };
        let local_edit = self.local.as_ref().is_some_and(|local| local.actor == commit.actor);
        let selection = if self.story_draft.is_some() {
            None
        } else if let Some(transaction) = &commit.proposal.story_transaction {
            let pending = self.pending_story_selection.as_ref().filter(|selection| local_edit && selection.sequence == commit.proposal.sequence);
            story_sync::selection_after(st, &self.replica, &replica, transaction, pending)
        } else if commit.proposal.edits.iter().any(|op| {
            matches!(op, Operation::SetAutomergeTextBatchActive { story, .. } if st.selection.text.is_some_and(|sel| sel.story == *story && sel.cell.is_none()))
        }) {
            story_sync::remap_selection(st, &self.replica, &replica)
        } else {
            text_selection::after(st, &self.replica, &replica, &commit.proposal.edits, local_edit)
        };
        let previous_selection = st.selection.text;
        let acknowledged = self.matches_pending(&commit);
        let front = if acknowledged { self.pending.pop_front() } else { None };
        let keeps_visible_draft = !self.story_queue_blocked
            && !self.pending.is_empty()
            && front
                .as_ref()
                .and_then(|p| p.story_transaction.as_ref())
                .is_some_and(|front| Some(front) == commit.proposal.story_transaction.as_ref());
        if let Err(error) =
            self.storage.append_confirmed(self.history.iter().map(|(c, _)| c), &commit, bytes.len(), &self.pending, self.next_sequence)
        {
            if let Some(front) = front {
                self.pending.push_front(front);
            }
            return Err(error);
        }
        #[cfg(unix)]
        if let Some(journal) = &mut self.server_journal
            && let Err(error) = journal.append(&commit)
        {
            let message = format!("Could not store the confirmed edit: {error}");
            self.stop_with_error(&message);
            return Err(message);
        }
        let created_layer = (local_edit
            && self.layer_entry == Some((commit.proposal.sequence, st.active_layer))
            && matches!(commit.proposal.edits.as_slice(), [Operation::Layer { edit: LayerEdit::Create { .. } }]))
        .then_some(LayerId(doc.next_id));
        let confirmed = Arc::new(doc);
        if let Some(mut previous) = self.story_draft.take() {
            if keeps_visible_draft {
                // The editor already applied this exact change, including any dependent drafts.
                previous.confirmed = confirmed;
                self.story_draft = Some(previous);
            } else {
                let rebased = if self.story_queue_blocked && !self.pending.is_empty() {
                    Err("Queued changes are paused.".into())
                } else {
                    story_sync::rebase(&confirmed, &replica, self.storage.identity().actor(), &self.pending)
                };
                self.install_rebased(st, rebased, confirmed.clone(), Some(previous), &replica);
            }
        } else {
            install_document(st, confirmed.clone());
        }
        let installed_confirmed = self.story_draft.as_ref().map_or(&st.doc, |draft| &draft.confirmed);
        self.model_size = Some((Arc::downgrade(installed_confirmed), model_size));
        if let Some(layer) = created_layer {
            st.active_layer = layer;
        }
        if let Some(selection) = selection {
            st.selection.text = Some(selection);
        }
        self.update_typing_selection(previous_selection, st.selection.text);
        self.replica = replica;
        if acknowledged {
            self.sending_proposal = None;
            self.last_send = None;
            self.story_sender.acknowledge(&self.pending);
            if self.pending.is_empty() {
                self.story_queue_blocked = false;
            }
        }
        if local_edit {
            self.pending_story_selection = None;
            self.layer_entry = None;
            if self.frame_entry.as_ref().is_some_and(|entry| entry.sequence == commit.proposal.sequence) {
                self.confirm_frame_entry(session);
            } else {
                self.select_own_creations(session, &commit.proposal.edits);
            }
        }
        self.revision = commit.revision;
        self.history_bytes += bytes.len();
        self.history_artwork += bytes.artwork_size;
        self.committed_sequences.insert((commit.actor, commit.proposal.sequence), self.history.len());
        self.history.push((commit, bytes));
        self.inverses.push(inverse);
        Ok(())
    }

    fn install_rebased(
        &mut self,
        st: &mut designcraft_engine::DocState,
        result: Result<(Arc<Document>, Option<story_sync::Draft>), String>,
        confirmed: Arc<Document>,
        previous: Option<story_sync::Draft>,
        replica: &Replica,
    ) {
        match result {
            Ok((display, draft)) => {
                let selection = previous
                    .as_ref()
                    .and_then(|previous| story_sync::remap_selection(st, &previous.visible, draft.as_ref().map_or(replica, |view| &view.visible)));
                install_document(st, display);
                if let Some(selection) = selection {
                    st.selection.text = Some(selection);
                }
                self.story_draft = draft;
            }
            Err(_) => {
                if let Some(mut previous) = previous {
                    previous.confirmed = confirmed;
                    self.story_draft = Some(previous);
                } else {
                    install_document(st, confirmed);
                }
                self.block_story_queue();
            }
        }
    }

    fn select_own_creations(&self, session: &mut Session, edits: &[Operation]) {
        let ids: Vec<_> = edits
            .iter()
            .filter_map(|op| match op {
                Operation::Create { id, .. } | Operation::CreateGraphic { id, .. } | Operation::CreateText { id, .. } => self.replica.local_id(id),
                _ => None,
            })
            .collect();
        if !ids.is_empty()
            && let Some(st) = session.document_by_uid_mut(self.doc_uid)
        {
            st.selection = designcraft_doc::Selection::items(ids);
        }
    }

    fn relay_presence(&self, actor: ActorId, state: Presence) {
        let Ok(bytes) = encode_for_protocol(Message::Presence { actor, state }, self.protocol) else { return };
        if let Mode::Host { delivery, .. } = &self.mode {
            for (&peer, delivery) in delivery {
                if peer != actor && delivery.announced {
                    let _ = self.transport.send_to(peer, bytes.clone());
                }
            }
        }
    }
}

fn install_confirmed(st: &mut designcraft_engine::DocState, doc: Document) {
    install_document(st, Arc::new(doc));
}

fn install_document(st: &mut designcraft_engine::DocState, doc: Arc<Document>) {
    if doc == st.doc {
        return;
    }
    st.selection.items.retain(|id| doc.item(*id).is_some());
    st.selection.anchors.retain(|(id, _, _)| doc.item(*id).is_some());
    st.selection.key = st.selection.key.filter(|id| doc.item(*id).is_some());
    remap_page_selection(&st.doc, &doc, &mut st.selection);
    st.selection.text = st.selection.text.filter(|text| doc.text_story(text.story, text.cell).is_some());
    st.selection.cells = st.selection.cells.filter(|cells| doc.story(cells.story).is_some());
    st.history.undo.clear();
    st.history.redo.clear();
    if doc.layer(st.active_layer).is_none() {
        st.active_layer = doc.working_layer();
    }
    st.doc = doc;
    st.bump_revision();
}

fn remap_page_selection(before: &Document, after: &Document, selection: &mut Selection) {
    selection.page = selection.page.and_then(|index| before.page(index)).and_then(|page| {
        let (spread, index) = page_location(after, page.id)?;
        Some(after.first_page_of_spread(spread) + index)
    });
    selection.guides.retain_mut(|guide| {
        if !matches!(guide.spread, SpreadRef::Doc(_)) {
            return true;
        }
        let Some(page) = before.spread(guide.spread).and_then(|spread| spread.pages.get(guide.page)) else { return false };
        let Some((spread, index)) = page_location(after, page.id) else { return false };
        guide.spread = SpreadRef::Doc(spread);
        guide.page = index;
        guide.index < after.spreads[spread].pages[index].guides.len()
    });
}

fn page_location(document: &Document, id: PageId) -> Option<(usize, usize)> {
    document.spreads.iter().enumerate().find_map(|(spread, pages)| pages.pages.iter().position(|page| page.id == id).map(|page| (spread, page)))
}

fn operation_story(operation: &Operation) -> Option<designcraft_doc::StoryId> {
    match operation {
        Operation::Text { story, .. } | Operation::SetTextBatchActive { story, .. } | Operation::SetAutomergeTextBatchActive { story, .. } => {
            Some(*story)
        }
        _ => None,
    }
}
