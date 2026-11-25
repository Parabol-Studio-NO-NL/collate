use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use designcraft_collab::PROTOCOL_VERSION;
use designcraft_collab_core::{ActorId, Event, Identity, Peer, Session as Transport};
use designcraft_doc::Document;
use web_time::Instant;

use super::attachments::{self, BlobCursor, BlobDownload, Blobs};
use super::seed::{SEED_CHUNK_BYTES, SeedDownload, SeedUpload};
use super::{Message, Packet, Room, encode, seed_document};

const TIMEOUT: Duration = Duration::from_secs(90);
const SEND_BURST: usize = 32;

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

    pub(super) fn poll(&mut self) -> Result<Option<Document>, String> {
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

    fn read_seed(&mut self, bytes: &[u8]) -> Result<Option<Document>, String> {
        if attachments::is_part(bytes) {
            self.blobs.append(bytes, designcraft_collab::artwork::MAX_ARTWORK_BYTES)?;
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
                        let document = seed_document::restore_bundle(&bytes, |id| self.blobs.load(id))?;
                        self.blobs.finish()?;
                        Ok(document)
                    })
                    .transpose()
            }
            Message::Incompatible => Err("The host could not provide a compatible seed document.".into()),
            _ => Ok(None),
        }
    }
}

#[derive(Default)]
pub(super) struct SeedDelivery {
    upload: Option<SeedUpload>,
    peers: BTreeMap<ActorId, (BlobCursor, Option<u32>)>,
    finished: BTreeSet<ActorId>,
}

impl SeedDelivery {
    pub(super) fn progress(&self, blobs: &Blobs) -> Option<attachments::ArtworkProgress> {
        self.peers.values().find_map(|(cursor, _)| cursor.progress(blobs))
    }

    pub(super) fn enqueue(&mut self, archive: &[u8], recipient: ActorId) -> Result<(), String> {
        if self.finished.contains(&recipient) {
            return Ok(());
        }
        if self.upload.is_none() {
            self.upload = Some(SeedUpload::new(archive).map_err(|error| error.to_string())?);
        }
        // A retry from an already queued peer does not restart a live stream.
        self.peers.entry(recipient).or_default();
        Ok(())
    }

    pub(super) fn stop(&mut self, peer: ActorId) {
        self.peers.remove(&peer);
        self.finished.remove(&peer);
    }

    pub(super) fn advance(&mut self, transport: &Transport, blobs: &Blobs) {
        let Some(upload) = self.upload.as_ref() else { return };
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
}

impl Room {
    pub(super) fn provide_seed(&mut self, peer: ActorId) {
        if self.peers.contains_key(&peer) && self.seed_delivery.enqueue(&self.storage.archive, peer).is_err() {
            self.reply(peer, Message::Incompatible);
        }
    }
}
