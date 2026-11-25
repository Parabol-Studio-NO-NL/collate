//! An unattended host for one document. Uses the desktop room controller without a window or display.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use designcraft_collab::artwork::{BlobId, MAX_ARTWORK_BYTES};
use designcraft_collab_core::Identity;
use designcraft_doc::Document;
use designcraft_engine::{DocState, Session};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Commit, MAX_HISTORY, MAX_HISTORY_BYTES, Mode, Room, Storage, Transport, attachments::Blobs, fingerprint, replay, seed_document};

const VERSION: u16 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u16,
    identity: [u8; 32],
    access: [u8; 32],
    seed: [u8; 32],
}

/// Records the protocol may access inside an operator-selected room.
#[derive(Clone, Copy)]
pub enum RoomFile {
    Header,
    Seed,
    Blob([u8; 32]),
    Revision(u64),
    Invitation,
}

/// Exclusive storage for a single room. The operator supplies the implementation.
/// Writes are durable. The store creates each revision and the immutable seed exactly once.
pub trait RoomFiles: Send + Sync {
    /// Read one record, failing if its length exceeds the limit.
    fn read(&self, file: RoomFile, limit: u64) -> Result<Vec<u8>, String>;
    fn write(&self, file: RoomFile, bytes: &[u8]) -> Result<(), String>;
    /// List committed revisions in ascending order, failing if their count exceeds the limit.
    fn revisions(&self, limit: usize) -> Result<Vec<u64>, String>;
    fn clear_invitation(&self) -> Result<(), String>;
}

pub(super) struct Journal {
    files: Arc<dyn RoomFiles>,
    blobs: Blobs,
    blob_bytes: usize,
}

impl Journal {
    fn new(files: Arc<dyn RoomFiles>) -> Self {
        Self { files, blobs: Blobs::new(), blob_bytes: 0 }
    }

    fn header(&self) -> Result<Header, String> {
        let bytes = self.files.read(RoomFile::Header, 4096)?;
        let header: Header = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if header.version != VERSION {
            return Err("Unsupported room data version.".into());
        }
        Ok(header)
    }

    fn seed(&mut self, header: &Header) -> Result<Document, String> {
        let bytes = self.files.read(RoomFile::Seed, super::seed::MAX_SEED_BYTES as u64)?;
        let doc = seed_document::restore_bundle(&bytes, |id| self.load_blob(id))?;
        if fingerprint(&doc)? != header.seed {
            return Err("The room seed differs from the saved room identity.".into());
        }
        Ok(doc)
    }

    fn check_blob_space(&self, id: BlobId) -> Result<(), String> {
        if !id.valid() || self.blob_bytes.saturating_add(id.length as usize) > MAX_ARTWORK_BYTES {
            return Err("The saved artwork exceeds the room limit.".into());
        }
        Ok(())
    }

    fn remember(&mut self, id: BlobId, data: Arc<Vec<u8>>) {
        self.blob_bytes += id.length as usize;
        self.blobs.insert(id, data);
    }

    fn verify(id: BlobId, data: &[u8]) -> Result<(), String> {
        if data.len() != id.length as usize || <[u8; 32]>::from(Sha256::digest(data)) != id.digest {
            return Err("Saved artwork does not match its content hash.".into());
        }
        Ok(())
    }

    fn load_blob(&mut self, id: BlobId) -> Result<Arc<Vec<u8>>, String> {
        if let Some(data) = self.blobs.get(&id) {
            return Ok(data.clone());
        }
        self.check_blob_space(id)?;
        let data = Arc::new(self.files.read(RoomFile::Blob(id.digest), u64::from(id.length))?);
        Self::verify(id, &data)?;
        self.remember(id, data.clone());
        Ok(data)
    }

    fn save_blobs(&mut self, blobs: Blobs) -> Result<(), String> {
        for (id, data) in blobs {
            if self.blobs.contains_key(&id) {
                continue;
            }
            self.check_blob_space(id)?;
            Self::verify(id, &data)?;
            // Replace an orphan left by an interrupted write before publishing its reference.
            self.files.write(RoomFile::Blob(id.digest), &data)?;
            self.remember(id, data);
        }
        Ok(())
    }

    pub(super) fn append(&mut self, commit: &Commit) -> Result<(), String> {
        self.save_blobs(commit.proposal.blobs()?)?;
        self.files.write(RoomFile::Revision(commit.revision), &serde_json::to_vec(commit).map_err(|e| e.to_string())?)
    }

    fn history(&mut self) -> Result<Vec<Commit>, String> {
        let mut commits = Vec::new();
        let mut remaining = MAX_HISTORY_BYTES as u64;
        for revision in self.files.revisions(MAX_HISTORY)? {
            let bytes = self.files.read(RoomFile::Revision(revision), remaining)?;
            remaining -= bytes.len() as u64;
            let mut commit: Commit = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if commit.revision != revision || revision != commits.len() as u64 + 1 {
                return Err("The room journal has a missing or mismatched revision.".into());
            }
            let mut attached = Blobs::new();
            for id in commit.proposal.blob_refs() {
                attached.insert(id, self.load_blob(id)?);
            }
            commit.proposal.attach(&attached)?;
            commits.push(commit);
        }
        Ok(commits)
    }

    fn load(&mut self) -> Result<(Header, Document, replay::RoomReplay), String> {
        let header = self.header()?;
        let seed = self.seed(&header)?;
        let recovered = replay::restore(&seed, &seed, self.history()?)?;
        Ok((header, seed, recovered))
    }
}

/// Initialize the supplied storage with a standalone document.
pub fn init(files: Arc<dyn RoomFiles>, mut doc: Document) -> Result<(), String> {
    seed_document::prepare_local_copy(&mut doc)?;
    doc.compact_assets();
    let (bytes, blobs) = seed_document::bundle(&doc)?;
    let header =
        Header { version: VERSION, seed: fingerprint(&doc)?, identity: Identity::generate().to_bytes(), access: Identity::generate().to_bytes() };
    let mut journal = Journal::new(files);
    journal.save_blobs(blobs)?;
    journal.files.write(RoomFile::Seed, &bytes)?;
    journal.files.write(RoomFile::Header, &serde_json::to_vec(&header).map_err(|e| e.to_string())?)
}

/// Replace the room access token while the server is stopped.
pub fn rotate_invite(files: Arc<dyn RoomFiles>) -> Result<(), String> {
    let journal = Journal::new(files);
    let mut header = journal.header()?;
    journal.files.clear_invitation()?;
    header.access = Identity::generate().to_bytes();
    journal.files.write(RoomFile::Header, &serde_json::to_vec(&header).map_err(|e| e.to_string())?)
}

/// Reconstruct a standalone snapshot from the last confirmed revision.
pub fn snapshot(files: Arc<dyn RoomFiles>) -> Result<Document, String> {
    Ok(Journal::new(files).load()?.2.document)
}

pub struct Server {
    room: Room,
    session: Session,
    files: Arc<dyn RoomFiles>,
    published: Option<String>,
}

impl Server {
    /// Reopen the last confirmed revision and advertise the room on its configured port.
    pub fn open(files: Arc<dyn RoomFiles>, relay: bool, port: u16) -> Result<Self, String> {
        let mut journal = Journal::new(files.clone());
        let (header, seed, recovered) = journal.load()?;
        journal.files.clear_invitation()?;
        let storage = Storage::ephemeral(seed, header.identity)?;
        let transport = Transport::persistent_host("Collate room server", relay, storage.identity(), port, header.access)?;
        let state = DocState::new(recovered.document, None);
        let mut room = Room::new(transport, Mode::Host { delivery: BTreeMap::new() }, storage, state.uid, header.seed);
        room.revision = recovered.history.len() as u64;
        room.history = recovered.history;
        room.inverses = recovered.inverses;
        room.history_bytes = recovered.history_bytes;
        room.replica = recovered.replica;
        room.server_journal = Some(journal);
        let mut session = Session::new();
        session.add_document(state);
        Ok(Self { session, room, files, published: None })
    }

    /// Process a bounded batch of events. No UI frame or GPU is required.
    pub fn poll(&mut self) -> Result<Duration, String> {
        self.room.poll(&mut self.session);
        if self.room.fatal {
            return Err(self.room.error.clone().unwrap_or_else(|| "Room stopped.".into()));
        }
        if let Some(invite) = &self.room.invitation
            && self.published.as_ref() != Some(invite)
        {
            self.files.write(RoomFile::Invitation, invite.as_bytes())?;
            self.published = Some(invite.clone());
        }
        Ok(if self.room.peers.is_empty() { Duration::from_millis(100) } else { Duration::from_millis(10) })
    }
}

// Exercise the executable’s actual lock, filesystem and atomic writes with the native editor.
#[cfg(test)]
#[path = "../../../../apps/collate-room-server/src/directory.rs"]
pub(super) mod directory;
#[cfg(test)]
mod tests;
