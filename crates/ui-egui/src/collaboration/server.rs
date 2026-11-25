//! An unattended host for one document. Uses the desktop room controller without a window or display.
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use craft_atomic::{AtomicFs, NewFilePermissions, RealFs, WriteOptions};
use designcraft_collab::artwork::{BlobId, MAX_ARTWORK_BYTES};
use designcraft_collab_core::Identity;
use designcraft_doc::{Document, build::NewDocument};
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

/// The exclusive directory lock survives atomic replacement of individual files.
/// It drops automatically when the server exits or crashes.
pub(super) struct Journal {
    data: PathBuf,
    _lock: File,
    blobs: Blobs,
    blob_bytes: usize,
}

fn io_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn read_capped(path: impl AsRef<Path>, limit: u64) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len > limit {
        return Err(io::Error::other("File exceeds its size limit."));
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len as usize).map_err(io::Error::other)?;
    file.take(len + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != len {
        return Err(io::Error::other("File changed during read."));
    }
    Ok(bytes)
}

fn clear_invitation(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}

fn private_files() -> WriteOptions {
    WriteOptions::default().with_new_file_permissions(NewFilePermissions::Private)
}

impl Journal {
    fn lock(root: &Path) -> Result<Self, String> {
        let metadata = fs::symlink_metadata(root).map_err(io_error)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err("Use a private room directory (permissions 0700).".into());
        }
        let root = fs::canonicalize(root).map_err(io_error)?;
        let lock = File::open(&root).map_err(io_error)?;
        lock.try_lock().map_err(|error| format!("Could not exclusively lock the room directory: {error}"))?;
        Ok(Self { data: root.join("state"), _lock: lock, blobs: Blobs::new(), blob_bytes: 0 })
    }

    fn header(&self) -> Result<Header, String> {
        let bytes = read_capped(self.data.join("room.json"), 4096).map_err(io_error)?;
        let header: Header = serde_json::from_slice(&bytes).map_err(io_error)?;
        if header.version != VERSION {
            return Err("Unsupported room data version.".into());
        }
        Ok(header)
    }

    fn seed(&mut self, header: &Header) -> Result<Document, String> {
        let bytes = read_capped(self.data.join("seed"), super::seed::MAX_SEED_BYTES as u64).map_err(io_error)?;
        let doc = seed_document::restore_bundle(&bytes, |id| self.load_blob(id))?;
        if fingerprint(&doc)? != header.seed {
            return Err("The room seed differs from the saved room identity.".into());
        }
        Ok(doc)
    }

    fn blob_path(&self, id: BlobId) -> PathBuf {
        self.data.join("blobs").join(URL_SAFE_NO_PAD.encode(id.digest))
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
        let data = Arc::new(read_capped(self.blob_path(id), u64::from(id.length)).map_err(io_error)?);
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
            private_files().atomic_write(&self.blob_path(id), &data).map_err(io_error)?;
            self.remember(id, data);
        }
        Ok(())
    }

    pub(super) fn append(&mut self, commit: &Commit) -> Result<(), String> {
        self.save_blobs(commit.proposal.blobs()?)?;
        let path = self.data.join("history").join(format!("{}.json", commit.revision));
        private_files().write_new_with(&path, |file| serde_json::to_writer(file, commit).map_err(io::Error::other)).map_err(io_error)
    }

    fn history(&mut self) -> Result<Vec<Commit>, String> {
        let history = self.data.join("history");
        let mut revisions = BTreeMap::new();
        for entry in fs::read_dir(&history).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let path = entry.path();
            // Ignore interrupted atomic-write staging files, whose names do not end in .json.
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let revision = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok()).ok_or("Invalid journal revision name.")?;
            if revisions.insert(revision, path).is_some() || revisions.len() > MAX_HISTORY {
                return Err("Unexpected number of room revisions.".into());
            }
        }
        let mut commits = Vec::new();
        let mut remaining = MAX_HISTORY_BYTES as u64;
        for (revision, path) in revisions {
            let bytes = read_capped(path, remaining).map_err(io_error)?;
            remaining -= bytes.len() as u64;
            let mut commit: Commit = serde_json::from_slice(&bytes).map_err(io_error)?;
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

/// Initialize a new private room. The optional original document is imported once.
pub fn init(root: &Path, seed_path: Option<&Path>) -> Result<(), String> {
    let mut doc = if let Some(path) = seed_path {
        let input = read_capped(path, MAX_ARTWORK_BYTES as u64 + 8 * 1024 * 1024).map_err(io_error)?;
        designcraft_format::load(&input).map_err(io_error)?
    } else {
        Document::new(&NewDocument::default())
    };
    doc.compact_assets();
    let (bytes, blobs) = seed_document::bundle(&doc)?;
    let header =
        Header { version: VERSION, seed: fingerprint(&doc)?, identity: Identity::generate().to_bytes(), access: Identity::generate().to_bytes() };
    match fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io_error(error)),
    }
    let mut journal = Journal::lock(root)?;
    for entry in fs::read_dir(root).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        // A crash during initialization may leave an unpublished staging directory.
        if !entry.file_name().to_string_lossy().starts_with(".collate-init-") || !entry.file_type().map_err(io_error)?.is_dir() {
            return Err("Initialize in an empty directory. Use serve to resume an existing room.".into());
        }
    }
    let state = journal.data;
    let stage = tempfile::Builder::new().prefix(".collate-init-").tempdir_in(root).map_err(io_error)?;
    journal.data = stage.path().to_owned();
    fs::DirBuilder::new().mode(0o700).create(journal.data.join("history")).map_err(io_error)?;
    fs::DirBuilder::new().mode(0o700).create(journal.data.join("blobs")).map_err(io_error)?;
    journal.save_blobs(blobs)?;
    private_files().write_new(&journal.data.join("seed"), &bytes).map_err(io_error)?;
    private_files().write_new(&journal.data.join("room.json"), &serde_json::to_vec(&header).map_err(io_error)?).map_err(io_error)?;
    fs::rename(stage.path(), &state).map_err(io_error)?;
    RealFs.sync_dir(root).map_err(io_error)
}

/// Replace the room access token while the server is stopped.
pub fn rotate_invite(root: &Path) -> Result<(), String> {
    let journal = Journal::lock(root)?;
    let mut header = journal.header()?;
    clear_invitation(&root.join("invitation.txt"))?;
    header.access = Identity::generate().to_bytes();
    private_files().atomic_write(&journal.data.join("room.json"), &serde_json::to_vec(&header).map_err(io_error)?).map_err(io_error)
}

/// Export a standalone snapshot. Shared comments and edit history stay in the room directory.
pub fn export(root: &Path, output: &Path) -> Result<(), String> {
    let mut journal = Journal::lock(root)?;
    let (_, _, recovered) = journal.load()?;
    private_files()
        .write_new_with(output, |file| designcraft_format::write(file, &recovered.document, None).map_err(io::Error::other))
        .map_err(io_error)
}

pub struct Server {
    room: Room,
    session: Session,
    invitation_file: PathBuf,
    published: Option<String>,
}

impl Server {
    /// Reopen the last confirmed revision and advertise the room on its configured port.
    pub fn open(root: &Path, relay: bool, port: u16) -> Result<Self, String> {
        let mut journal = Journal::lock(root)?;
        let (header, seed, recovered) = journal.load()?;
        let invitation_file = root.join("invitation.txt");
        clear_invitation(&invitation_file)?;
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
        Ok(Self { session, room, invitation_file, published: None })
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
            private_files().atomic_write(&self.invitation_file, invite.as_bytes()).map_err(io_error)?;
            self.published = Some(invite.clone());
        }
        Ok(if self.room.peers.is_empty() { Duration::from_millis(100) } else { Duration::from_millis(10) })
    }
}

#[cfg(test)]
mod tests;
