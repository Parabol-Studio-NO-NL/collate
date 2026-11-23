//! Native app-thread room controller. Network messages never enter the command registry.

mod controls;
mod edit_wire;
mod history;
mod join;
mod presence;
mod replay;
mod seed;
mod seed_document;
mod storage;
#[cfg(test)]
mod tests;
mod text_selection;
mod typing;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use designcraft_collab::{Operation, PROTOCOL_VERSION, Replica, layers::LayerEdit};
use designcraft_collab_core::{ActorId, Event, MAX_PAYLOAD, Peer, Session as Transport};
use designcraft_doc::{Document, LayerId, PageId, Selection, SpreadRef};
use designcraft_engine::{DeferredEdit, Session};
use serde::{Deserialize, Serialize};
use web_time::Instant;

use self::{
    edit_wire::{SendCursor, WireEdit},
    history::HistoryAction,
    join::{PendingJoin, SeedDelivery},
    presence::Presence,
    seed::{SeedAdvertisement, SeedChunk, SeedDownload},
    storage::Storage,
};
use crate::DesignApp;

const MAX_HISTORY: usize = 1024;
const MAX_HISTORY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    sequence: u64,
    base: u64,
    edits: Vec<Operation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history_action: Option<HistoryAction>,
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
    Declined { sequence: u64 },
    Incompatible,
    RequestSeed,
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

fn encode(message: Message) -> Result<Vec<u8>, String> {
    let bytes = encode_packet(message)?;
    if bytes.len() > MAX_PAYLOAD {
        return Err("The edit is too large to share in one step.".into());
    }
    Ok(bytes)
}

fn encode_packet(message: Message) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&Packet { version: PROTOCOL_VERSION, message }).map_err(|_| "Could not encode the collaboration message.".into())
}

fn encode_edit(message: Message) -> Result<WireEdit, String> {
    let artwork = matches!(&message, Message::Propose(proposal) | Message::Commit(Commit { proposal, .. }) if proposal.edits.iter().any(Operation::changes_artwork));
    let bytes = if artwork { encode_packet(message)? } else { encode(message)? };
    WireEdit::new(bytes)
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
}

struct Room {
    transport: Transport,
    storage: Storage,
    history: Vec<(Commit, WireEdit)>,
    inverses: Vec<Option<Vec<Operation>>>,
    history_bytes: usize,
    seed_delivery: SeedDelivery,
    receiving_edits: BTreeMap<ActorId, SeedDownload>,
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
    pending: Option<Proposal>,
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

#[derive(Default)]
pub(crate) struct Collaboration {
    pub(crate) open: bool,
    name: String,
    join_code: String,
    relay: bool,
    room: Option<Room>,
    joining: Option<PendingJoin>,
    message: Option<String>,
    comment: String,
    reply_to: Option<designcraft_collab::SharedId>,
    confirm_discard: bool,
}

impl DesignApp {
    pub(crate) fn poll_collaboration(&mut self, ctx: &egui::Context) {
        self.poll_seed_download(ctx);
        let Some(room) = self.collaboration.room.as_mut() else { return };
        let Some(st) = self.session.documents().iter().find(|st| st.uid == room.doc_uid) else {
            self.session.defer_document_edits(None);
            self.collaboration.room = None;
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
        self.session.set_deferred_edit_ready(room.can_edit());
        room.flush_typing(&mut self.session);
        ctx.request_repaint_after(Duration::from_millis(40));
    }

    pub(crate) fn collaboration_blocks_close(&self) -> bool {
        self.session.has_deferred_edit() || self.collaboration.room.as_ref().is_some_and(|room| room.pending.is_some() || room.typing.is_some())
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

    fn install_downloaded_seed(&mut self, joining: PendingJoin, doc: Document) -> Result<(), String> {
        let seed = fingerprint(&doc)?;
        let storage = Storage::ephemeral(doc.clone(), joining.identity.to_bytes())?;
        let state = designcraft_engine::DocState::new(doc, None);
        let uid = state.uid;
        self.session.add_document(state);
        let mut room = Room::new(joining.transport, Mode::Guest { ready: false, hello_sent: false }, storage, uid, seed);
        room.local = joining.local;
        room.host = Some(joining.host);
        room.online = true;
        room.peers = joining.peers;
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
        let host = Transport::invitation_host(&c.join_code)?;
        if room.host != Some(host) {
            return Err("Use a current invitation from the same room host.".into());
        }
        let connection = Transport::join_with_identity(&c.join_code, &c.name, room.storage.identity())?;
        room.transport = connection;
        room.mode = Mode::Guest { ready: false, hello_sent: false };
        room.online = false;
        room.peers.clear();
        room.presence.clear();
        room.error = None;
        room.fatal = false;
        room.last_send = None;
        room.receiving_edits.clear();
        room.sending_proposal = None;
        c.join_code.clear();
        Ok(())
    }

    fn connect_collaboration(&mut self, host: bool) -> Result<(), String> {
        let st = self.session.active().ok_or("Open the saved document before connecting.")?;
        if st.interaction.is_some() || st.preview_stash.is_some() {
            return Err("Finish the current document edit before connecting.".into());
        }
        let c = &mut self.collaboration;
        let guest_of = if host { None } else { Some(Transport::invitation_host(&c.join_code)?) };
        if cfg!(target_os = "macos") && self.session.recovery_dir.is_some() && (st.path.is_none() || st.is_dirty()) {
            return Err("Save the seed document before starting or joining a room.".into());
        }
        let (storage, saved) = Storage::open(self.session.recovery_dir.as_deref(), st.path.as_deref(), &st.doc, guest_of)?;
        let seed = fingerprint(&storage.seed)?;
        let uid = st.uid;
        let recovered = replay::restore(&storage.seed, &st.doc, saved.confirmed)?;
        let revision = recovered.history.len() as u64;
        if saved.pending.as_ref().is_some_and(|pending| host || pending.base > revision || pending.sequence >= saved.next_sequence) {
            return Err("The local collaboration journal has an invalid pending edit.".into());
        }
        let (transport, mode) = if host {
            (Transport::host_with_identity(&c.name, c.relay, storage.identity())?, Mode::Host { delivery: BTreeMap::new() })
        } else {
            (Transport::join_with_identity(&c.join_code, &c.name, storage.identity())?, Mode::Guest { ready: false, hello_sent: false })
        };
        storage.write(recovered.history.iter().map(|(commit, _)| commit), saved.pending.as_ref(), saved.next_sequence)?;
        let durable = storage.is_durable();
        if let Some(st) = self.session.active_mut() {
            install_confirmed(st, recovered.document);
        }
        c.join_code.clear();
        let mut room = Room::new(transport, mode, storage, uid, seed);
        room.history = recovered.history;
        room.inverses = recovered.inverses;
        room.history_bytes = recovered.history_bytes;
        room.replica = recovered.replica;
        room.host = guest_of;
        room.next_sequence = saved.next_sequence;
        room.revision = revision;
        room.pending = saved.pending;
        c.room = Some(room);
        c.message = None;
        self.session.defer_document_edits(Some(uid));
        self.session.set_deferred_edit_durable(durable);
        Ok(())
    }
}

fn fingerprint(doc: &Document) -> Result<[u8; 32], String> {
    let mut canonical = doc.clone();
    canonical.compact_assets();
    seed_document::fingerprint(&canonical)
}

impl Room {
    fn new(transport: Transport, mode: Mode, storage: Storage, doc_uid: u64, seed: [u8; 32]) -> Self {
        let replica = Replica::new(&storage.seed);
        Self {
            transport,
            mode,
            storage,
            doc_uid,
            seed,
            replica,
            history: Vec::new(),
            inverses: Vec::new(),
            history_bytes: 0,
            seed_delivery: SeedDelivery::default(),
            receiving_edits: BTreeMap::new(),
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
            pending: None,
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

    #[cfg(target_os = "macos")]
    fn enable_recovery(&mut self, recovery_dir: Option<&std::path::Path>, path: &str) {
        if self.storage.is_durable() || recovery_dir.is_none() || self.recovery_path_checked.as_deref() == Some(path) {
            return;
        }
        let Mode::Guest { .. } = self.mode else { return };
        let Some(host) = self.host else { return };
        self.recovery_path_checked = Some(path.into());
        if let Err(error) =
            self.storage.enable_journal(recovery_dir, path, host, self.history.iter().map(|(c, _)| c), self.pending.as_ref(), self.next_sequence)
        {
            self.error = Some(error);
        }
    }

    fn can_edit(&self) -> bool {
        !self.fatal
            && self.local.is_some()
            && self.pending.is_none()
            && match self.mode {
                Mode::Host { .. } => self.online && self.history.len() < MAX_HISTORY && self.history_bytes < MAX_HISTORY_BYTES,
                Mode::Guest { ready, .. } => ready || self.has_joined && !self.online && self.storage.is_durable(),
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
        self.advance_delivery();
        self.seed_delivery.advance(&self.transport);
        self.advance_proposal();
    }

    fn advance_proposal(&mut self) {
        let (Mode::Guest { ready: true, .. }, Some(pending)) = (&self.mode, &self.pending) else { return };
        if self.sending_proposal.is_none() && self.last_send.is_none_or(|last| last.elapsed() >= Duration::from_secs(2)) {
            self.sending_proposal = match encode_edit(Message::Propose(pending.clone())) {
                Ok(wire) => Some((wire, SendCursor::default())),
                Err(error) => return self.stop_with_error(&error),
            };
        }
        let Some((wire, cursor)) = self.sending_proposal.as_mut() else { return };
        for _ in 0..8 {
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
            Event::Ready { local, host } => {
                self.local = Some(local);
                self.online = true;
                self.host = Some(host);
                self.last_send = None;
            }
            Event::Invitation(code) => self.invitation = Some(code),
            Event::PeerJoined(peer) => {
                self.peers.insert(peer.actor, peer);
            }
            Event::PeerLeft(actor) => {
                self.peers.remove(&actor);
                self.presence.remove(&actor);
                self.seed_delivery.stop(actor);
                self.receiving_edits.remove(&actor);
                if let Mode::Host { delivery, .. } = &mut self.mode {
                    delivery.remove(&actor);
                }
            }
            Event::Message { actor, bytes } => self.on_packet(session, actor, &bytes),
            Event::Disconnected => {
                self.online = false;
                self.peers.clear();
                self.presence.clear();
                self.receiving_edits.clear();
                self.sending_proposal = None;
                if matches!(self.mode, Mode::Guest { .. }) {
                    self.mode = Mode::Guest { ready: false, hello_sent: false };
                }
            }
            Event::Failed(error) => self.stop_with_error(&error),
        }
    }

    fn on_packet(&mut self, session: &mut Session, actor: ActorId, bytes: &[u8]) {
        if self.fatal {
            return;
        }
        let Ok(packet) = serde_json::from_slice::<Packet>(bytes) else { return };
        if packet.version != PROTOCOL_VERSION {
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
        let Ok(Packet { version: PROTOCOL_VERSION, message }) = serde_json::from_slice(&bytes) else { return self.drop_transfer(actor) };
        match message {
            Message::Propose(ref proposal) if self.serving(actor) && proposal.edits.iter().any(Operation::changes_artwork) => {
                self.on_guest_message(session, actor, message);
            }
            Message::Commit(ref commit) if self.host == Some(actor) && commit.proposal.edits.iter().any(Operation::changes_artwork) => {
                self.on_host_message(session, message);
            }
            _ => self.drop_transfer(actor),
        }
    }

    fn drop_transfer(&mut self, actor: ActorId) {
        self.receiving_edits.remove(&actor);
        if matches!(self.mode, Mode::Guest { .. }) {
            self.mode = Mode::Guest { ready: false, hello_sent: false };
            self.error = Some("Requesting missing edits from the host.".into());
        }
    }

    fn on_guest_message(&mut self, session: &mut Session, actor: ActorId, message: Message) {
        match message {
            Message::RequestSeed => self.provide_seed(actor),
            Message::Hello { seed, revision } => {
                if seed != self.seed || revision > self.revision {
                    self.reply(actor, Message::Incompatible);
                } else if let Mode::Host { delivery, .. } = &mut self.mode {
                    self.receiving_edits.remove(&actor);
                    delivery.insert(actor, Delivery { sent: revision as usize, cursor: SendCursor::default(), announced: false });
                }
            }
            Message::Propose(proposal) if self.serving(actor) => {
                let sequence = proposal.sequence;
                if self.admit(session, actor, proposal).is_err() {
                    self.reply(actor, Message::Declined { sequence });
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
            Message::Commit(commit) => {
                if commit.revision > self.revision {
                    if commit.revision != self.revision + 1 {
                        self.mode = Mode::Guest { ready: false, hello_sent: false };
                        self.error = Some("Requesting missing edits from the host.".into());
                        return;
                    }
                    if let Err(error) = self.commit(session, commit.clone()) {
                        self.stop_with_error(&error);
                        return;
                    }
                }
                if self.matches_pending(&commit)
                    && let Err(error) = self.clear_pending()
                {
                    self.stop_with_error(&error);
                }
            }
            Message::CaughtUp { revision } if revision == self.revision => {
                self.has_joined = true;
                self.error = None;
                self.mode = Mode::Guest { ready: true, hello_sent: true };
            }
            Message::Declined { sequence } => self.decline_pending(sequence),
            Message::Presence { actor, state } if state.is_valid() && self.peers.contains_key(&actor) => {
                self.presence.insert(actor, state);
            }
            Message::Incompatible => self.stop_with_error("Open the same saved document as the host and try again."),
            _ => {}
        }
    }

    fn stop_with_error(&mut self, error: &str) {
        self.error = Some(error.into());
        self.fatal = true;
    }

    fn serving(&self, actor: ActorId) -> bool {
        matches!(&self.mode, Mode::Host { delivery, .. } if delivery.get(&actor).is_some_and(|d| d.announced))
    }

    fn reply(&self, actor: ActorId, message: Message) {
        if let Ok(bytes) = encode(message) {
            let _ = self.transport.send_to(actor, bytes);
        }
    }

    fn advance_delivery(&mut self) {
        match &mut self.mode {
            Mode::Host { delivery } => {
                for (&actor, state) in delivery {
                    for _ in 0..8 {
                        let Some((_, wire)) = self.history.get(state.sent) else { break };
                        match wire.send_next(&mut state.cursor, |frame| self.transport.send_to(actor, frame)) {
                            Ok(true) => {
                                state.sent += 1;
                                state.cursor = SendCursor::default();
                            }
                            Ok(false) => {}
                            Err(_) => break,
                        }
                    }
                    if state.sent == self.history.len()
                        && !state.announced
                        && let Ok(bytes) = encode(Message::CaughtUp { revision: self.revision })
                        && self.transport.send_to(actor, bytes).is_ok()
                    {
                        state.announced = true;
                    }
                }
            }
            Mode::Guest { hello_sent, .. } => {
                if !*hello_sent
                    && self.online
                    && let Ok(bytes) = encode(Message::Hello { seed: self.seed, revision: self.revision })
                    && self.transport.submit(bytes).is_ok()
                {
                    *hello_sent = true;
                }
            }
        }
    }

    fn capture_edit(&mut self, session: &mut Session, edit: DeferredEdit) -> Result<(), String> {
        let local = self.local.as_ref().ok_or("The collaboration connection is still starting.")?;
        self.error = None;
        let selections = edit.before_selection.text.zip(edit.after_selection.text);
        let edits = self
            .replica
            .capture_selected(&edit.before, &edit.after, &local.actor.to_string(), self.next_sequence, selections)
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
        let actor = self.local.as_ref().ok_or("The collaboration connection is still starting.")?.actor;
        let proposal = Proposal { sequence: self.next_sequence, base: self.revision, edits, history_action };
        self.next_sequence = self.next_sequence.checked_add(1).ok_or("The shared room reached its edit limit.")?;
        let wire = encode_edit(Message::Propose(proposal.clone()))?;
        if matches!(self.mode, Mode::Host { .. }) {
            self.admit(session, actor, proposal)
        } else if self.pending.is_none() {
            self.storage.write(self.history.iter().map(|(c, _)| c), Some(&proposal), self.next_sequence)?;
            self.pending = Some(proposal);
            self.sending_proposal = Some((wire, SendCursor::default()));
            self.last_send = None;
            Ok(())
        } else {
            Err("Wait for the host to confirm your previous edit.".into())
        }
    }

    fn admit(&mut self, session: &mut Session, actor: ActorId, mut proposal: Proposal) -> Result<(), String> {
        if !matches!(self.mode, Mode::Host { .. }) {
            return Err("Only the host can confirm edits.".into());
        }
        if let Some(index) = self.history.iter().position(|(old, _)| old.actor == actor && old.proposal.sequence == proposal.sequence) {
            if let Mode::Host { delivery } = &mut self.mode
                && let Some(state) = delivery.get_mut(&actor).filter(|state| state.sent > index)
            {
                state.sent = index;
                state.cursor = SendCursor::default();
            }
            return Ok(());
        }
        if proposal.base > self.revision || proposal.sequence == 0 || self.history.len() >= MAX_HISTORY {
            return Err("The edit has an invalid revision or the room is full.".into());
        }
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
        if proposal.edits.is_empty() {
            return Err("The proposal has no edits.".into());
        }
        let author = actor.to_string();
        if !proposal.edits.iter().all(|edit| edit.is_authored_by(&author) && edit.is_from_sequence(proposal.sequence)) {
            return Err("Could not verify the edit author.".into());
        }
        let changed = self.history.iter().skip(proposal.base as usize);
        if changed.flat_map(|(old, _)| &old.proposal.edits).any(|old| proposal.edits.iter().any(|edit| edit.conflicts(old))) {
            return Err("A collaborator changed the same object property first.".into());
        }
        self.commit(session, Commit { revision: self.revision + 1, actor, proposal, correction })
    }

    fn matches_pending(&self, commit: &Commit) -> bool {
        self.local.as_ref().is_some_and(|local| commit.actor == local.actor)
            && self.pending.as_ref().is_some_and(|p| p.sequence == commit.proposal.sequence)
    }

    fn clear_pending(&mut self) -> Result<(), String> {
        if self.pending.is_some() {
            self.storage.write(self.history.iter().map(|(c, _)| c), None, self.next_sequence)?;
            self.pending = None;
            self.sending_proposal = None;
            self.layer_entry = None;
            if self.frame_entry.take().is_some() {
                self.pause_typing();
            }
        }
        Ok(())
    }

    fn decline_pending(&mut self, sequence: u64) {
        let Some(proposal) = self.pending.as_ref().filter(|proposal| proposal.sequence == sequence) else { return };
        let submitted = typing::submitted_typing(proposal);
        if let Err(error) = self.clear_pending() {
            self.stop_with_error(&error);
            return;
        }
        self.error = Some("The host could not accept this edit. The confirmed document is still open.".into());
        self.preserve_rejected_typing(submitted);
    }

    fn commit(&mut self, session: &mut Session, commit: Commit) -> Result<(), String> {
        history::verify_text_compensation(&commit, self.history.iter().map(|(original, _)| original), &self.inverses)?;
        let bytes = encode_edit(Message::Commit(commit.clone()))?;
        if self.history.len() >= MAX_HISTORY || self.history_bytes.saturating_add(bytes.len()) > MAX_HISTORY_BYTES {
            return Err("The shared room reached its history limit.".into());
        }
        let st = session.document_by_uid_mut(self.doc_uid).ok_or("The shared document closed.")?;
        let mut doc = (*st.doc).clone();
        let mut replica = self.replica.clone();
        let inverse = replica.inverse_patch_batch(&doc, &commit.proposal.edits).map_err(|error| error.to_string())?;
        replica.apply(&mut doc, &commit.proposal.edits).map_err(|error| error.to_string())?;
        if commit.proposal.edits.iter().any(Operation::changes_artwork) {
            seed_document::check_artwork(&doc)?;
        }
        // Every committed checkpoint must still fit the model bound during journal replay.
        seed_document::model_bytes(&doc)?;
        let local_edit = self.local.as_ref().is_some_and(|local| local.actor == commit.actor);
        let selection = text_selection::after(st, &self.replica, &replica, &commit.proposal.edits, local_edit);
        let previous_selection = st.selection.text;
        let acknowledged = self.matches_pending(&commit);
        let pending = if acknowledged { None } else { self.pending.as_ref() };
        self.storage.write(self.history.iter().map(|(c, _)| c).chain(std::iter::once(&commit)), pending, self.next_sequence)?;
        let created_layer = (local_edit
            && self.layer_entry == Some((commit.proposal.sequence, st.active_layer))
            && matches!(commit.proposal.edits.as_slice(), [Operation::Layer { edit: LayerEdit::Create { .. } }]))
        .then_some(LayerId(doc.next_id));
        install_confirmed(st, doc);
        if let Some(layer) = created_layer {
            st.active_layer = layer;
        }
        if let Some(selection) = selection {
            st.selection.text = Some(selection);
        }
        self.update_typing_selection(previous_selection, st.selection.text);
        self.replica = replica;
        if acknowledged {
            self.pending = None;
            self.sending_proposal = None;
        }
        if local_edit {
            self.layer_entry = None;
            if self.frame_entry.as_ref().is_some_and(|entry| entry.sequence == commit.proposal.sequence) {
                self.confirm_frame_entry(session);
            } else {
                self.select_own_creations(session, &commit.proposal.edits);
            }
        }
        self.revision = commit.revision;
        self.history_bytes += bytes.len();
        self.history.push((commit, bytes));
        self.inverses.push(inverse);
        Ok(())
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
        let Ok(bytes) = encode(Message::Presence { actor, state }) else { return };
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
    if doc == *st.doc {
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
    st.doc = Arc::new(doc);
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
        Operation::Text { story, .. } | Operation::SetTextBatchActive { story, .. } => Some(*story),
        _ => None,
    }
}
