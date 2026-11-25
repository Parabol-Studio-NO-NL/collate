use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use designcraft_collab::{PROTOCOL_VERSION, artwork::BlobId};
use designcraft_collab_core::{ActorId, Event, Identity, Peer, Session as Transport};
use designcraft_doc::Document;
use web_time::Instant;

use super::attachments::{self, BlobCursor, BlobDownload, Blobs};
use super::seed::{SEED_CHUNK_BYTES, SeedDownload, SeedUpload};
use super::{Message, Mode, Packet, Room, encode, seed_document};

const TIMEOUT: Duration = Duration::from_secs(90);
const SEND_BURST: usize = 32;
const ORIGINAL_BURST: usize = 2;

pub(super) struct JoinedSeed {
    pub(super) document: Document,
    pub(super) originals: OriginalDownloads,
}

pub(super) struct PendingJoin {
    pub(super) transport: Transport,
    pub(super) identity: Identity,
    pub(super) host: ActorId,
    pub(super) local: Option<Peer>,
    pub(super) peers: BTreeMap<ActorId, Peer>,
    download: Option<SeedDownload>,
    blobs: BlobDownload,
    deadline: Instant,
    requested: bool,
}

impl PendingJoin {
    pub(super) fn artwork_progress(&self) -> Option<attachments::ArtworkProgress> {
        self.blobs.progress()
    }

    pub(super) fn start(invite: &str, name: &str) -> Result<Self, String> {
        let host = Transport::invitation_host(invite)?;
        let identity = Identity::generate();
        let transport = Transport::join_with_identity(invite, name, Identity::from_bytes(&identity.to_bytes()))?;
        Ok(Self {
            transport,
            identity,
            host,
            local: None,
            peers: BTreeMap::new(),
            download: None,
            blobs: BlobDownload::default(),
            deadline: Instant::now() + TIMEOUT,
            requested: false,
        })
    }

    pub(super) fn poll(&mut self) -> Result<Option<JoinedSeed>, String> {
        for _ in 0..32 {
            let Some(event) = self.transport.try_event() else { break };
            match event {
                Event::Ready { local, .. } => {
                    self.local = Some(local);
                    self.requested = false;
                    self.download = None;
                    self.blobs = BlobDownload::default();
                }
                Event::PeerJoined(peer) => {
                    self.peers.insert(peer.actor, peer);
                }
                Event::PeerLeft(actor) => {
                    self.peers.remove(&actor);
                }
                Event::Disconnected => {
                    self.local = None;
                    self.peers.clear();
                    self.download = None;
                    self.blobs = BlobDownload::default();
                    self.requested = false;
                }
                Event::Failed(error) => return Err(error),
                Event::Message { actor, bytes } if actor == self.host => {
                    if let Some(doc) = self.read_seed(&bytes)? {
                        return Ok(Some(doc));
                    }
                }
                _ => {}
            }
        }
        if Instant::now() >= self.deadline {
            return Err("The document download timed out. Check the connection and try again.".into());
        }
        if self.local.is_some() && !self.requested && self.transport.submit(encode(Message::RequestSeed)?).is_ok() {
            self.requested = true;
        }
        Ok(None)
    }

    fn read_seed(&mut self, bytes: &[u8]) -> Result<Option<JoinedSeed>, String> {
        if attachments::is_part(bytes) {
            self.blobs.append_seed(bytes, designcraft_collab::artwork::MAX_ARTWORK_BYTES, seed_document::MAX_SEED_BLOBS)?;
            self.deadline = Instant::now() + TIMEOUT;
            return Ok(None);
        }
        let Ok(packet) = serde_json::from_slice::<Packet>(bytes) else { return Ok(None) };
        if packet.version != PROTOCOL_VERSION {
            return Err("This room uses a different collaboration protocol.".into());
        }
        match packet.message {
            Message::SeedOffer(offer) if self.download.is_none() => {
                self.download = Some(SeedDownload::new(offer).map_err(|error| error.to_string())?);
                Ok(None)
            }
            Message::SeedPart(chunk) => {
                let download = self.download.as_mut().ok_or("The host sent a document chunk before the transfer began.")?;
                let bytes = download.append(chunk).map_err(|error| error.to_string())?;
                self.deadline = Instant::now() + TIMEOUT;
                bytes
                    .map(|bytes| {
                        let (document, missing) = if seed_document::is_preview_seed(&bytes) {
                            seed_document::restore_preview_seed(&bytes, |id| self.blobs.load(id))?
                        } else {
                            (seed_document::restore_bundle(&bytes, |id| self.blobs.load(id))?, seed_document::OriginalTargets::new())
                        };
                        self.blobs.finish()?;
                        Ok(JoinedSeed { document, originals: OriginalDownloads { missing, ..Default::default() } })
                    })
                    .transpose()
            }
            Message::Incompatible => Err("The host could not provide a compatible seed document.".into()),
            _ => Ok(None),
        }
    }
}

#[derive(Default)]
pub(super) struct OriginalDownloads {
    missing: seed_document::OriginalTargets,
    transfer: Option<OriginalTransfer>,
    verification: Option<OriginalVerification>,
    failed: bool,
}

struct OriginalTransfer {
    id: BlobId,
    bytes: BlobDownload,
    requested: Instant,
}

struct OriginalVerification {
    id: BlobId,
    result: mpsc::Receiver<Result<Arc<Vec<u8>>, String>>,
}

impl OriginalDownloads {
    pub(super) fn is_pending(&self) -> bool {
        !self.missing.is_empty()
    }

    pub(super) fn disconnect(&mut self) {
        self.transfer = None;
        self.failed = false;
    }

    pub(super) fn progress(&self) -> Option<attachments::ArtworkProgress> {
        self.transfer.as_ref()?.bytes.progress()
    }

    fn advance(&mut self, transport: &Transport, connected: bool) -> Result<(), String> {
        if let Some(check) = &self.verification {
            let result = match check.result.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return Ok(()),
                Err(mpsc::TryRecvError::Disconnected) => Err("Could not verify the shared original.".into()),
            };
            let id = check.id;
            self.verification = None;
            let bytes = result.inspect_err(|_| self.failed = true)?;
            if let Some(assets) = self.missing.remove(&id) {
                for asset in assets {
                    if !asset.data.install_verified_original(bytes.clone()) {
                        return Err("Could not install the verified shared original.".into());
                    }
                }
            }
        }
        if !connected || self.failed {
            return Ok(());
        }
        if let Some(transfer) = &self.transfer {
            if transfer.bytes.expired() || transfer.bytes.reserved() == 0 && transfer.requested.elapsed() >= TIMEOUT {
                self.failed = true;
                self.transfer = None;
                return Err("The original artwork download timed out. Reconnect to try again.".into());
            }
            return Ok(());
        }
        if let Some((&id, _)) = self.missing.first_key_value()
            && transport.submit(encode(Message::RequestOriginal { blob: id })?).is_ok()
        {
            self.transfer = Some(OriginalTransfer { id, bytes: BlobDownload::default(), requested: Instant::now() });
        }
        Ok(())
    }

    fn receive(&mut self, bytes: &[u8]) -> Result<(), String> {
        let Some(transfer) = &mut self.transfer else { return Ok(()) };
        match transfer.bytes.append_original(bytes, transfer.id) {
            Ok(false) => return Ok(()),
            Err(error) => {
                self.transfer = None;
                self.failed = true;
                return Err(error);
            }
            Ok(true) => {}
        }
        let Some(OriginalTransfer { id, mut bytes, .. }) = self.transfer.take() else { return Ok(()) };
        let assets = self.missing.get(&id).ok_or("The room sent an unrequested original.")?;
        let asset = assets.first().ok_or("The room sent an unrequested original.")?.clone();
        if assets.iter().any(|other| other.mime != asset.mime) {
            self.failed = true;
            return Err("The room describes the same original with different formats.".into());
        }
        let (sender, result) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("collate-original".into())
            .spawn(move || {
                let outcome = bytes.load(id).and_then(|body| {
                    seed_document::verify_original(&asset, &body)?;
                    Ok(body)
                });
                let _ = sender.send(outcome);
            })
            .map_err(|_| {
                self.failed = true;
                "Could not verify the shared original."
            })?;
        self.verification = Some(OriginalVerification { id, result });
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct SeedDelivery {
    upload: Option<SeedUpload>,
    preparing: Option<mpsc::Receiver<Result<seed_document::PreviewSeed, String>>>,
    preview: Option<seed_document::PreviewSeed>,
    failed: bool,
    peers: BTreeMap<ActorId, (BlobCursor, Option<u32>)>,
    finished: BTreeSet<ActorId>,
    originals: BTreeMap<ActorId, (BlobId, u32)>,
}

impl SeedDelivery {
    pub(super) fn progress(&self, blobs: &Blobs) -> Option<attachments::ArtworkProgress> {
        let blobs = self.preview.as_ref().map_or(blobs, |preview| &preview.eager);
        self.peers.values().find_map(|(cursor, _)| cursor.progress(blobs)).or_else(|| {
            self.originals.values().next().map(|(id, offset)| attachments::ArtworkProgress {
                sending: true,
                bytes: *offset as usize,
                total: id.length as usize,
            })
        })
    }

    pub(super) fn enqueue(&mut self, archive: &[u8], document: &Arc<Document>, recipient: ActorId) -> Result<(), String> {
        if self.finished.contains(&recipient) {
            return Ok(());
        }
        if self.failed {
            return Err("Could not prepare the room previews.".into());
        }
        if self.upload.is_none() && self.preparing.is_none() {
            if seed_document::use_preview_seed(document) {
                let document = document.clone();
                let (sender, receiver) = mpsc::sync_channel(1);
                std::thread::Builder::new()
                    .name("collate-room-previews".into())
                    .spawn(move || {
                        let _ = sender.send(seed_document::preview_seed(&document));
                    })
                    .map_err(|_| "Could not prepare the room previews.")?;
                self.preparing = Some(receiver);
            } else {
                self.upload = Some(SeedUpload::new(archive).map_err(|error| error.to_string())?);
            }
        }
        // A retry from an already queued peer does not restart a live stream.
        self.peers.entry(recipient).or_default();
        Ok(())
    }

    pub(super) fn stop(&mut self, peer: ActorId) {
        self.peers.remove(&peer);
        self.finished.remove(&peer);
        self.originals.remove(&peer);
    }

    pub(super) fn advance(&mut self, transport: &Transport, blobs: &Blobs) {
        if let Some(preparing) = &self.preparing {
            let received = match preparing.try_recv() {
                Ok(preview) => preview,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => Err("Could not prepare the room previews.".into()),
            };
            self.preparing = None;
            match received.and_then(|preview| SeedUpload::new(&preview.archive).map(|upload| (upload, preview)).map_err(|error| error.to_string())) {
                Ok((upload, preview)) => {
                    self.upload = Some(upload);
                    self.preview = Some(preview);
                }
                Err(_) => {
                    self.failed = true;
                    if let Ok(bytes) = encode(Message::Incompatible) {
                        for &peer in self.peers.keys() {
                            let _ = transport.send_to(peer, bytes.clone());
                        }
                    }
                    self.peers.clear();
                }
            }
        }
        let Some(upload) = self.upload.as_ref() else { return };
        let blobs = self.preview.as_ref().map_or(blobs, |preview| &preview.eager);
        self.peers.retain(|&peer, (binary, offset)| {
            for _ in 0..SEND_BURST {
                match binary.next(blobs, |part| transport.send_to(peer, part)) {
                    Some(Ok(())) => continue,
                    Some(Err(_)) => return true,
                    None => break,
                }
            }
            if !binary.finished(blobs) {
                return true;
            }

            if offset.is_none() {
                let Ok(bytes) = encode(Message::SeedOffer(upload.advertisement())) else { return false };
                if transport.send_to(peer, bytes).is_err() {
                    return true;
                }
                *offset = Some(0);
            }
            let Some(mut cursor) = *offset else { return false };
            for _ in 0..SEND_BURST {
                let Some(chunk) = upload.chunk(cursor) else {
                    self.finished.insert(peer);
                    return false;
                };
                let next = cursor.saturating_add(SEED_CHUNK_BYTES).min(upload.advertisement().length);
                let Ok(bytes) = encode(Message::SeedPart(chunk)) else { return false };
                if transport.send_to(peer, bytes).is_err() {
                    return true;
                }
                cursor = next;
                *offset = Some(cursor);
            }
            true
        });
    }

    fn enqueue_original(&mut self, peer: ActorId, id: BlobId) {
        if self.preview.as_ref().is_some_and(|preview| preview.originals.contains_key(&id)) {
            self.originals.entry(peer).or_insert((id, 0));
        }
    }

    fn advance_originals(&mut self, transport: &Transport) {
        let Some(preview) = &self.preview else { return };
        self.originals.retain(|&peer, (id, offset)| {
            let Some(original) = preview.originals.get(id) else { return false };
            for _ in 0..ORIGINAL_BURST {
                let Some((bytes, next)) = attachments::original_part(*id, original, *offset) else { return false };
                if transport.send_to(peer, bytes).is_err() {
                    return true;
                }
                *offset = next;
            }
            *offset < id.length
        });
    }
}

impl Room {
    pub(super) fn provide_seed(&mut self, peer: ActorId) {
        if self.peers.contains_key(&peer) && self.seed_delivery.enqueue(&self.storage.archive, &self.storage.seed, peer).is_err() {
            self.reply(peer, Message::Incompatible);
        }
    }

    pub(super) fn provide_original(&mut self, peer: ActorId, id: BlobId) {
        self.seed_delivery.enqueue_original(peer, id);
    }

    pub(super) fn advance_original_delivery(&mut self) {
        if let Err(error) = self.originals.advance(&self.transport, self.online && matches!(self.mode, Mode::Guest { ready: true, .. })) {
            self.error = Some(error);
        }
        self.seed_delivery.advance_originals(&self.transport);
    }

    pub(super) fn on_original_packet(&mut self, actor: ActorId, bytes: &[u8]) -> bool {
        if !attachments::is_original_part(bytes) {
            return false;
        }
        if self.host == Some(actor)
            && matches!(self.mode, Mode::Guest { ready: true, .. })
            && let Err(error) = self.originals.receive(bytes)
        {
            self.error = Some(error);
        }
        true
    }
}
