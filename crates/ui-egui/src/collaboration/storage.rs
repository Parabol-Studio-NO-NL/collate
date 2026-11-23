use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use designcraft_collab_core::{ActorId, Identity};
use designcraft_doc::Document;
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc};

use super::{Commit, Proposal, seed_document};

const JOURNAL_VERSION: u16 = 1;

#[cfg(target_os = "macos")]
use designcraft_collab_core::journal::{Journal, RecordId, RecordLock, Role};
#[cfg(target_os = "macos")]
use sha2::{Digest, Sha256};

#[derive(Serialize)]
struct Record<'a> {
    version: u16,
    identity: &'a [u8; 32],
    seed: &'a str,
    next_sequence: u64,
    confirmed: Vec<&'a Commit>,
    pending: Option<&'a Proposal>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SavedRoom {
    version: u16,
    identity: [u8; 32],
    seed: SavedSeed,
    pub(super) next_sequence: u64,
    pub(super) confirmed: Vec<Commit>,
    pub(super) pending: Option<Proposal>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SavedSeed {
    Archive(String),
    Model(Box<Document>),
}

pub(super) struct Storage {
    identity: [u8; 32],
    pub(super) seed: Arc<Document>,
    pub(super) archive: Vec<u8>,
    encoded_archive: String,
    #[cfg(target_os = "macos")]
    durable: Option<Durable>,
}

#[cfg(target_os = "macos")]
struct Durable {
    journal: Journal,
    record: RecordId,
    _lease: RecordLock,
}

impl Storage {
    pub(super) fn ephemeral(document: Document, identity: [u8; 32]) -> Result<Self, String> {
        let archive = seed_document::encode(&document)?;
        Ok(Self {
            identity,
            seed: Arc::new(document),
            encoded_archive: STANDARD_NO_PAD.encode(&archive),
            archive,
            #[cfg(target_os = "macos")]
            durable: None,
        })
    }

    pub(super) fn open(
        recovery_dir: Option<&Path>,
        document_path: Option<&str>,
        document: &Document,
        guest_of: Option<ActorId>,
    ) -> Result<(Self, SavedRoom), String> {
        #[cfg(target_os = "macos")]
        let durable = match (recovery_dir.and_then(Path::parent), document_path) {
            (Some(root), Some(path)) => Some(Durable::new(root, path, guest_of)?),
            _ => None,
        };
        #[cfg(not(target_os = "macos"))]
        let _ = (recovery_dir, document_path, guest_of);

        #[cfg(target_os = "macos")]
        let existing = match durable.as_ref() {
            Some(durable) => durable.journal.load(durable.record).map_err(|error| error.to_string())?,
            None => None,
        };
        #[cfg(not(target_os = "macos"))]
        let existing: Option<Vec<u8>> = None;

        let saved = if let Some(bytes) = existing {
            let saved: SavedRoom = serde_json::from_slice(&bytes).map_err(|_| "The local collaboration journal could not be read.")?;
            if saved.version != JOURNAL_VERSION || saved.next_sequence == 0 || saved.confirmed.len() > super::MAX_HISTORY {
                return Err("The local collaboration journal needs a compatible app version.".into());
            }
            saved
        } else {
            let mut seed = document.clone();
            seed.compact_assets();
            SavedRoom {
                version: JOURNAL_VERSION,
                identity: Identity::generate().to_bytes(),
                seed: SavedSeed::Model(Box::new(seed)),
                next_sequence: 1,
                confirmed: Vec::new(),
                pending: None,
            }
        };
        let (seed, archive) = match &saved.seed {
            SavedSeed::Model(seed) => ((**seed).clone(), seed_document::encode(seed)?),
            SavedSeed::Archive(encoded) => {
                let archive = STANDARD_NO_PAD.decode(encoded).map_err(|_| "The saved seed archive is invalid.")?;
                (seed_document::decode(&archive)?, archive)
            }
        };
        let storage = Self {
            identity: saved.identity,
            seed: Arc::new(seed),
            encoded_archive: STANDARD_NO_PAD.encode(&archive),
            archive,
            #[cfg(target_os = "macos")]
            durable,
        };
        Ok((storage, saved))
    }

    #[cfg(target_os = "macos")]
    pub(super) fn enable_journal<'a>(
        &mut self,
        recovery_dir: Option<&Path>,
        path: &str,
        host: ActorId,
        confirmed: impl Iterator<Item = &'a Commit>,
        pending: Option<&Proposal>,
        next_sequence: u64,
    ) -> Result<(), String> {
        let root = recovery_dir.and_then(Path::parent).ok_or("No collaboration recovery directory is configured.")?;
        let durable = Durable::new(root, path, Some(host))?;
        if durable.journal.load(durable.record).map_err(|error| error.to_string())?.is_some() {
            return Err("This path already has a saved room. Keep this copy under a different name for recovery.".into());
        }
        self.durable = Some(durable);
        if let Err(error) = self.write(confirmed, pending, next_sequence) {
            self.durable = None;
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn identity(&self) -> Identity {
        Identity::from_bytes(&self.identity)
    }

    pub(super) fn is_durable(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self.durable.is_some();
        #[cfg(not(target_os = "macos"))]
        false
    }

    pub(super) fn write<'a>(
        &self,
        confirmed: impl Iterator<Item = &'a Commit>,
        pending: Option<&Proposal>,
        next_sequence: u64,
    ) -> Result<(), String> {
        if !self.is_durable() {
            return Ok(());
        }
        let record = Record {
            version: JOURNAL_VERSION,
            identity: &self.identity,
            seed: &self.encoded_archive,
            next_sequence,
            confirmed: confirmed.collect(),
            pending,
        };
        let bytes = serde_json::to_vec(&record).map_err(|_| "The local collaboration journal could not be encoded.")?;
        #[cfg(target_os = "macos")]
        if let Some(durable) = &self.durable {
            durable.journal.store(durable.record, &bytes).map_err(|error| error.to_string())?;
        }
        #[cfg(not(target_os = "macos"))]
        let _ = bytes;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Durable {
    fn new(root: &Path, path: &str, guest_of: Option<ActorId>) -> Result<Self, String> {
        let canonical = designcraft_engine::io::canonicalize(path).map_err(|_| "Could not find the saved document for collaboration.")?;
        let mut hash = Sha256::new();
        hash.update(canonical.as_os_str().as_encoded_bytes());
        if let Some(host) = guest_of {
            hash.update(host.0);
        }
        let record = RecordId { room: hash.finalize().into(), role: if guest_of.is_some() { Role::Guest } else { Role::Host } };
        let journal = Journal::new(root.join("Collaboration"));
        let lease = journal.lock(record).map_err(|error| error.to_string())?;
        Ok(Self { journal, record, _lease: lease })
    }
}
