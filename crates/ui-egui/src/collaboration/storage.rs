use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use designcraft_collab_core::{ActorId, Identity};
use designcraft_doc::Document;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{collections::VecDeque, fmt, path::Path, sync::Arc};

use super::{Commit, MAX_QUEUED_STORY_EDITS, Proposal, attachments::Blobs, seed_document};
use designcraft_collab::artwork::BlobId;

const JOURNAL_VERSION: u16 = 1;
const QUEUED_OUTBOX: bool = cfg!(feature = "automerge-rooms");
const MAX_PENDING: usize = if QUEUED_OUTBOX { MAX_QUEUED_STORY_EDITS } else { 1 };
#[cfg(target_os = "macos")]
pub(super) const JOURNAL_DIRECTORY: &str = if cfg!(feature = "automerge-rooms") { "Collaboration-Protocol16" } else { "Collaboration" };

#[cfg(target_os = "macos")]
use designcraft_collab_core::journal::{Journal, MAX_RECORD_BYTES, RecordId, RecordLock, Role};
#[cfg(target_os = "macos")]
use sha2::{Digest, Sha256};

#[derive(Serialize)]
struct Record<'a> {
    version: u16,
    identity: &'a [u8; 32],
    seed: &'a str,
    next_sequence: u64,
    confirmed: Vec<&'a Commit>,
    #[serde(serialize_with = "serialize_pending")]
    pending: &'a VecDeque<Proposal>,
}

#[cfg(target_os = "macos")]
fn encode_record(record: &impl Serialize) -> Result<Vec<u8>, String> {
    struct RecordBytes(Vec<u8>);

    impl std::io::Write for RecordBytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_RECORD_BYTES.saturating_sub(self.0.len()) {
                return Err(std::io::ErrorKind::FileTooLarge.into());
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut output = RecordBytes(Vec::new());
    serde_json::to_writer(&mut output, record).map_err(|error| {
        if error.is_io() {
            "The local collaboration journal exceeds its size limit."
        } else {
            "The local collaboration journal could not be encoded."
        }
    })?;
    Ok(output.0)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SavedRoom {
    version: u16,
    identity: [u8; 32],
    seed: SavedSeed,
    pub(super) next_sequence: u64,
    pub(super) confirmed: Vec<Commit>,
    #[serde(default, deserialize_with = "deserialize_pending")]
    pub(super) pending: VecDeque<Proposal>,
}

#[cfg(target_os = "macos")]
#[derive(Serialize)]
struct StoryOutboxRecord<'a> {
    confirmed_revision: u64,
    next_sequence: u64,
    #[serde(serialize_with = "serialize_pending")]
    pending: &'a VecDeque<Proposal>,
}

#[cfg(any(target_os = "macos", all(test, feature = "automerge-rooms")))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedStoryOutbox {
    confirmed_revision: u64,
    next_sequence: u64,
    #[serde(deserialize_with = "deserialize_pending")]
    pending: VecDeque<Proposal>,
}

#[cfg(any(target_os = "macos", all(test, feature = "automerge-rooms")))]
impl SavedRoom {
    fn restore_story_outbox(&mut self, outbox: SavedStoryOutbox) -> Result<(), String> {
        // A combined write atomically records an ACK or discard with the same or
        // a newer sequence counter. A newer confirmed history also supersedes
        // the shadow, even when the last pending sequence did not change.
        if outbox.confirmed_revision == self.confirmed.len() as u64 && outbox.next_sequence > self.next_sequence {
            validate_story_outbox(outbox.confirmed_revision, &outbox.pending, outbox.next_sequence)?;
            self.pending = outbox.pending;
            self.next_sequence = outbox.next_sequence;
        }
        Ok(())
    }
}

fn serialize_pending<S: Serializer>(pending: &VecDeque<Proposal>, serializer: S) -> Result<S::Ok, S::Error> {
    if pending.len() > MAX_PENDING {
        return Err(serde::ser::Error::custom("too many pending proposals"));
    }
    if QUEUED_OUTBOX { pending.serialize(serializer) } else { pending.front().serialize(serializer) }
}

fn deserialize_pending<'de, D: Deserializer<'de>>(deserializer: D) -> Result<VecDeque<Proposal>, D::Error> {
    struct PendingVisitor;

    impl<'de> de::Visitor<'de> for PendingVisitor {
        type Value = VecDeque<Proposal>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a pending proposal or a bounded queue of proposals")
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(VecDeque::new())
        }

        fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            let proposal = Proposal::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(VecDeque::from([proposal]))
        }

        fn visit_seq<S: de::SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
            if !QUEUED_OUTBOX {
                return Err(de::Error::custom("queued proposals require a compatible room version"));
            }
            let mut proposals = VecDeque::new();
            while proposals.len() < MAX_PENDING {
                let Some(proposal) = seq.next_element()? else { return Ok(proposals) };
                proposals.push_back(proposal);
            }
            if seq.next_element::<de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom("too many pending proposals"));
            }
            Ok(proposals)
        }
    }

    deserializer.deserialize_any(PendingVisitor)
}

fn validate_pending(pending: &VecDeque<Proposal>, next_sequence: u64) -> Result<(), String> {
    if pending.len() > MAX_PENDING {
        return Err("The collaboration journal has too many pending edits.".into());
    }
    if next_sequence == 0 {
        return Err("The collaboration journal has an invalid pending edit sequence.".into());
    }
    let mut previous = 0;
    for proposal in pending {
        if proposal.sequence <= previous || proposal.sequence >= next_sequence {
            return Err("The collaboration journal has an invalid pending edit sequence.".into());
        }
        previous = proposal.sequence;
    }
    Ok(())
}

fn validate_story_outbox(confirmed_revision: u64, pending: &VecDeque<Proposal>, next_sequence: u64) -> Result<(), String> {
    if !QUEUED_OUTBOX {
        return Err("Queued story recovery needs a compatible room version.".into());
    }
    validate_pending(pending, next_sequence)?;
    if pending.is_empty() || confirmed_revision > super::MAX_HISTORY as u64 {
        return Err("The queued story recovery record is invalid.".into());
    }
    for proposal in pending {
        // A pure story proposal cannot contain an artwork operation or an undo request.
        proposal.validate_story_transaction()?;
        if proposal.base > confirmed_revision || proposal.story_transaction.as_ref().is_none_or(|transaction| transaction.changes.is_empty()) {
            return Err("The queued story recovery record is invalid.".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn legacy_record(pending: Value) -> Result<SavedRoom, serde_json::Error> {
        serde_json::from_value(json!({
            "version": JOURNAL_VERSION, "identity": vec![0u8; 32], "seed": "",
            "next_sequence": 100, "confirmed": [], "pending": pending,
        }))
    }

    fn proposal(sequence: u64) -> Value {
        json!({"sequence": sequence, "base": 0, "edits": []})
    }

    fn encode(pending: &VecDeque<Proposal>) -> Result<Value, serde_json::Error> {
        serde_json::to_value(Record { version: JOURNAL_VERSION, identity: &[0; 32], seed: "", next_sequence: 100, confirmed: Vec::new(), pending })
    }

    #[test]
    fn older_journals_restore_and_current_protocol_writes_its_own_shape() {
        let empty = legacy_record(Value::Null).unwrap();
        let single = legacy_record(proposal(7)).unwrap();
        assert!(empty.pending.is_empty());
        assert_eq!(single.pending.front().unwrap().sequence, 7);
        let empty = encode(&empty.pending).unwrap();
        let single = encode(&single.pending).unwrap();
        if QUEUED_OUTBOX {
            assert_eq!(empty["pending"], json!([]));
            assert_eq!(single["pending"], json!([proposal(7)]));
        } else {
            assert_eq!(empty["pending"], Value::Null);
            assert_eq!(single["pending"], proposal(7));
        }
    }

    #[test]
    fn pending_edits_retain_order_and_always_obey_the_protocol_bound() {
        let encoded_queue = json!([proposal(7), proposal(9)]);
        if QUEUED_OUTBOX {
            let queue = legacy_record(encoded_queue.clone()).unwrap();
            assert_eq!(encode(&queue.pending).unwrap()["pending"], encoded_queue);
            let limit: Vec<_> = (1..=MAX_PENDING as u64).map(proposal).collect();
            let at_limit = legacy_record(json!(limit)).unwrap();
            assert_eq!(at_limit.pending.len(), MAX_PENDING);
            let over_limit: Vec<_> = (1..=MAX_PENDING as u64 + 1).map(proposal).collect();
            assert!(legacy_record(json!(over_limit)).is_err());
            let out_of_order = legacy_record(json!([proposal(9), proposal(7)])).unwrap();
            assert!(validate_pending(&out_of_order.pending, out_of_order.next_sequence).is_err());
        } else {
            assert!(legacy_record(encoded_queue).is_err());
            let mut too_many = legacy_record(proposal(7)).unwrap().pending;
            too_many.push_back(legacy_record(proposal(9)).unwrap().pending.pop_front().unwrap());
            assert!(encode(&too_many).is_err(), "a beta journal cannot silently drop a pending edit");
        }
    }

    #[cfg(feature = "automerge-rooms")]
    fn story_proposal(sequence: u64, base: u64) -> Value {
        json!({"sequence": sequence, "base": base, "edits": [], "story_transaction": {"changes": [{"story": 12, "bytes": "eA"}]}})
    }

    #[cfg(feature = "automerge-rooms")]
    fn shadow(confirmed_revision: u64, next_sequence: u64, pending: Value) -> SavedStoryOutbox {
        serde_json::from_value(json!({"confirmed_revision": confirmed_revision, "next_sequence": next_sequence, "pending": pending})).unwrap()
    }

    #[cfg(feature = "automerge-rooms")]
    #[test]
    fn only_a_newer_story_outbox_against_the_saved_revision_is_recovered() {
        let mut saved = legacy_record(Value::Null).unwrap();
        saved.confirmed.push(Commit {
            revision: 1,
            actor: ActorId([1; 32]),
            proposal: serde_json::from_value(proposal(6)).unwrap(),
            correction: None,
        });
        saved.restore_story_outbox(shadow(0, 101, json!([story_proposal(100, 0)]))).unwrap();
        assert!(saved.pending.is_empty(), "a newer confirmed revision supersedes the shadow");

        saved.restore_story_outbox(shadow(1, 100, json!([story_proposal(99, 1)]))).unwrap();
        assert!(saved.pending.is_empty(), "a same-revision discard supersedes the shadow");

        saved.restore_story_outbox(shadow(1, 102, json!([story_proposal(100, 1), story_proposal(101, 1)]))).unwrap();
        assert_eq!(saved.next_sequence, 102);
        assert_eq!(saved.pending.iter().map(|proposal| proposal.sequence).collect::<Vec<_>>(), [100, 101]);
    }

    #[cfg(feature = "automerge-rooms")]
    #[test]
    fn a_current_story_outbox_with_incompatible_proposals_keeps_the_combined_record() {
        let mut saved = legacy_record(story_proposal(7, 0)).unwrap();
        for pending in [json!([proposal(100)]), json!([story_proposal(100, 1)]), json!([story_proposal(101, 0)])] {
            assert!(saved.restore_story_outbox(shadow(0, 101, pending)).is_err());
            assert_eq!(saved.next_sequence, 100);
            assert_eq!(saved.pending.front().unwrap().sequence, 7);
        }
    }
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
    pub(super) seed_blobs: Blobs,
    encoded_archive: String,
    #[cfg(target_os = "macos")]
    durable: Option<Durable>,
}

#[derive(Clone, Copy)]
pub(super) enum OpenRoom {
    Active,
    NewHost([u8; 32]),
    SavedHost(Option<[u8; 32]>),
}

#[cfg(target_os = "macos")]
struct Durable {
    journal: Journal,
    record: RecordId,
    _lease: RecordLock,
    stored: std::cell::RefCell<std::collections::BTreeSet<BlobId>>,
    hosts: Option<HostRooms>,
}

#[cfg(target_os = "macos")]
struct HostRooms {
    index: RecordId,
    records: Vec<[u8; 32]>,
    staged: bool,
}

impl Storage {
    pub(super) fn ephemeral(document: Document, identity: [u8; 32]) -> Result<Self, String> {
        let (archive, seed_blobs) = seed_document::bundle(&document)?;
        Ok(Self {
            identity,
            seed_blobs,
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
        room: OpenRoom,
    ) -> Result<(Self, SavedRoom), String> {
        #[cfg(target_os = "macos")]
        let mut durable = match (recovery_dir.and_then(Path::parent), document_path) {
            (Some(root), Some(path)) => Some(Durable::new(root, path, guest_of)?),
            _ => None,
        };
        #[cfg(not(target_os = "macos"))]
        let _ = (recovery_dir, document_path, guest_of);

        let identity = Identity::generate();
        match room {
            OpenRoom::Active => {}
            OpenRoom::NewHost(previous) => {
                #[cfg(target_os = "macos")]
                durable.as_mut().ok_or("No collaboration recovery directory is configured.")?.stage_host(previous, identity.actor())?;
                #[cfg(not(target_os = "macos"))]
                return Err(unavailable_host_record(previous));
            }
            OpenRoom::SavedHost(previous) => {
                #[cfg(target_os = "macos")]
                durable.as_mut().ok_or("No collaboration recovery directory is configured.")?.select_saved_host(previous)?;
                #[cfg(not(target_os = "macos"))]
                return Err(unavailable_host_record(previous));
            }
        }

        #[cfg(target_os = "macos")]
        let existing = match durable.as_ref() {
            Some(durable) => durable.journal.load(durable.record).map_err(|error| error.to_string())?,
            None => None,
        };
        #[cfg(not(target_os = "macos"))]
        let existing: Option<Vec<u8>> = None;

        #[cfg(target_os = "macos")]
        if existing.is_none()
            && matches!(room, OpenRoom::Active)
            && durable.as_ref().and_then(|durable| durable.hosts.as_ref()).is_some_and(|hosts| hosts.records.len() > 1)
        {
            return Err("The active room is missing from local recovery.".into());
        }
        if existing.is_none() && matches!(room, OpenRoom::SavedHost(_)) {
            return Err("There is no saved room for this document.".into());
        }
        let mut saved = if let Some(bytes) = existing {
            let saved: SavedRoom = serde_json::from_slice(&bytes).map_err(|_| "The local collaboration journal could not be read.")?;
            if saved.version != JOURNAL_VERSION || saved.next_sequence == 0 || saved.confirmed.len() > super::MAX_HISTORY {
                return Err("The local collaboration journal needs a compatible app version.".into());
            }
            validate_pending(&saved.pending, saved.next_sequence)?;
            #[cfg(target_os = "macos")]
            let saved = if QUEUED_OUTBOX && let Some(durable) = durable.as_ref().filter(|durable| durable.record.role == Role::Guest) {
                durable.restore_story_outbox(saved)?
            } else {
                saved
            };
            saved
        } else {
            let mut seed = document.clone();
            seed.compact_assets();
            SavedRoom {
                version: JOURNAL_VERSION,
                identity: identity.to_bytes(),
                seed: SavedSeed::Model(Box::new(seed)),
                next_sequence: 1,
                confirmed: Vec::new(),
                pending: VecDeque::new(),
            }
        };
        let mut loaded = Blobs::new();
        let mut loaded_bytes = 0u64;
        let mut read = |id: BlobId| -> Result<Arc<Vec<u8>>, String> {
            if let Some(bytes) = loaded.get(&id) {
                return Ok(bytes.clone());
            }
            loaded_bytes += u64::from(id.length);
            if !id.valid() || loaded_bytes > designcraft_collab::artwork::MAX_ARTWORK_BYTES as u64 {
                return Err("Saved artwork exceeds the room limit.".into());
            }
            #[cfg(target_os = "macos")]
            let body = durable.as_ref().ok_or("The saved artwork is missing.")?.load_blob(id)?;
            #[cfg(not(target_os = "macos"))]
            let body = missing_blob(id)?;
            loaded.insert(id, body.clone());
            Ok(body)
        };
        let (seed, archive, seed_blobs) = match &saved.seed {
            SavedSeed::Model(seed) => {
                let (archive, blobs) = seed_document::bundle(seed)?;
                ((**seed).clone(), archive, blobs)
            }
            SavedSeed::Archive(encoded) => {
                let archive = STANDARD_NO_PAD.decode(encoded).map_err(|_| "The saved seed archive is invalid.")?;
                let mut blobs = Blobs::new();
                let doc = seed_document::restore_bundle(&archive, |id| {
                    let body = read(id)?;
                    blobs.insert(id, body.clone());
                    Ok(body)
                })?;
                (doc, archive, blobs)
            }
        };
        for proposal in saved.confirmed.iter_mut().map(|c| &mut c.proposal).chain(saved.pending.iter_mut()) {
            let mut blobs = Blobs::new();
            for id in proposal.blob_refs() {
                blobs.insert(id, read(id)?);
            }
            proposal.attach(&blobs)?;
        }
        let storage = Self {
            identity: saved.identity,
            seed_blobs,
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
        pending: &VecDeque<Proposal>,
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

    pub(super) fn host_room_id(&self) -> Option<[u8; 32]> {
        #[cfg(target_os = "macos")]
        return self.durable.as_ref().filter(|durable| durable.hosts.is_some()).map(|durable| durable.record.room);
        #[cfg(not(target_os = "macos"))]
        None
    }

    pub(super) fn activate(&mut self) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        if let Some(durable) = self.durable.as_ref()
            && let Some(hosts) = &durable.hosts
            && hosts.staged
        {
            self.write(std::iter::empty(), &VecDeque::new(), 1)?;
            let records: Vec<_> = hosts.records.iter().flatten().copied().collect();
            durable.journal.store(hosts.index, &records).map_err(|error| error.to_string())?;
            if let Some(hosts) = self.durable.as_mut().and_then(|durable| durable.hosts.as_mut()) {
                hosts.staged = false;
            }
        }
        Ok(())
    }

    /// Persist a guest's speculative story edits independently of the confirmed
    /// document. Changes to confirmed history and outbox removal use `write`.
    pub(super) fn write_story_outbox(&self, confirmed_revision: u64, pending: &VecDeque<Proposal>, next_sequence: u64) -> Result<(), String> {
        validate_story_outbox(confirmed_revision, pending, next_sequence)?;
        #[cfg(target_os = "macos")]
        if let Some(durable) = &self.durable {
            if durable.record.role != Role::Guest {
                return Err("Only a guest can keep a queued story recovery record.".into());
            }
            let bytes = encode_record(&StoryOutboxRecord { confirmed_revision, pending, next_sequence })?;
            durable.journal.store(durable.story_outbox_id(), &bytes).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn write<'a>(
        &self,
        confirmed: impl Iterator<Item = &'a Commit>,
        pending: &VecDeque<Proposal>,
        next_sequence: u64,
    ) -> Result<(), String> {
        validate_pending(pending, next_sequence)?;
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
        #[cfg(target_os = "macos")]
        let bytes = encode_record(&record)?;
        #[cfg(not(target_os = "macos"))]
        let bytes = serde_json::to_vec(&record).map_err(|_| "The local collaboration journal could not be encoded.")?;
        #[cfg(target_os = "macos")]
        if let Some(durable) = &self.durable {
            let mut blobs = self.seed_blobs.clone();
            for proposal in record.confirmed.iter().map(|c| &c.proposal).chain(record.pending) {
                blobs.extend(proposal.blobs()?);
            }
            super::attachments::total(&blobs)?;
            durable.write(&bytes, &blobs)?;
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
        let path_record = RecordId { room: hash.finalize().into(), role: if guest_of.is_some() { Role::Guest } else { Role::Host } };
        let journal = Journal::new(root.join(JOURNAL_DIRECTORY));
        // All host versions of the path share the existing writer lease. The
        // pointer and replacement can only be changed while that lease is held.
        let lease = journal.lock(path_record).map_err(|error| error.to_string())?;
        let mut record = path_record;
        let hosts = if path_record.role == Role::Host {
            let index = host_record(b"Collate host room index v1", path_record.room, &[]);
            let records = if let Some(bytes) = journal.load(index).map_err(|error| error.to_string())? {
                let (records, remaining) = bytes.as_chunks::<32>();
                if !remaining.is_empty() || records.first() != Some(&path_record.room) || records.len() < 2 {
                    return Err("The saved room index is invalid.".into());
                }
                records.to_vec()
            } else {
                vec![path_record.room]
            };
            record.room = *records.last().ok_or("The saved room index is invalid.")?;
            Some(HostRooms { index, records, staged: false })
        } else {
            None
        };
        Ok(Self { journal, record, _lease: lease, stored: Default::default(), hosts })
    }

    fn stage_host(&mut self, previous: [u8; 32], actor: ActorId) -> Result<(), String> {
        let hosts = self.hosts.as_mut().ok_or("Only the host can start a new room.")?;
        if previous != self.record.room {
            return Err("The saved room changed. Select Start sharing again to check this version.".into());
        }
        self.record = host_record(b"Collate host room version v1", hosts.index.room, &actor.0);
        if hosts.records.contains(&self.record.room) || self.journal.load(self.record).map_err(|error| error.to_string())?.is_some() {
            return Err("Could not create a new room identity. Try starting a new room again.".into());
        }
        hosts.records.push(self.record.room);
        hosts.staged = true;
        Ok(())
    }

    fn select_saved_host(&mut self, requested: Option<[u8; 32]>) -> Result<(), String> {
        let hosts = self.hosts.as_ref().ok_or("Only the host can recover a saved room.")?;
        if let Some(id) = requested {
            if !hosts.records.contains(&id) {
                return Err("The saved room is unavailable.".into());
            }
            self.record.room = id;
        } else if let Some(previous) = hosts.records.iter().rev().nth(1) {
            self.record.room = *previous;
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn host_record(domain: &[u8], seed: [u8; 32], identity: &[u8]) -> RecordId {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(seed);
    hash.update(identity);
    RecordId { room: hash.finalize().into(), role: Role::Host }
}

#[cfg(not(target_os = "macos"))]
fn unavailable_host_record(_: impl Sized) -> String {
    "Saved rooms are unavailable on this platform.".into()
}

#[cfg(not(target_os = "macos"))]
fn missing_blob(_: BlobId) -> Result<Arc<Vec<u8>>, String> {
    Err("The saved artwork is missing.".into())
}

#[cfg(target_os = "macos")]
const JOURNAL_PART: usize = 4 * 1024 * 1024;

#[cfg(target_os = "macos")]
impl Durable {
    fn story_outbox_id(&self) -> RecordId {
        let mut hash = Sha256::new();
        hash.update(b"Collate encrypted story outbox v1");
        hash.update(self.record.room);
        RecordId { room: hash.finalize().into(), role: self.record.role }
    }

    fn restore_story_outbox(&self, mut saved: SavedRoom) -> Result<SavedRoom, String> {
        if let Some(bytes) = self.journal.load(self.story_outbox_id()).map_err(|error| error.to_string())? {
            let outbox = serde_json::from_slice(&bytes).map_err(|_| "The queued story recovery record could not be read.")?;
            saved.restore_story_outbox(outbox)?;
        }
        Ok(saved)
    }

    fn blob_part(&self, id: BlobId, index: usize) -> RecordId {
        let mut hash = Sha256::new();
        hash.update(b"Collate encrypted artwork v1");
        hash.update(self.record.room);
        hash.update(id.digest);
        hash.update(id.length.to_be_bytes());
        hash.update((index as u64).to_be_bytes());
        RecordId { room: hash.finalize().into(), role: self.record.role }
    }

    fn load_blob(&self, id: BlobId) -> Result<Arc<Vec<u8>>, String> {
        let size = id.length as usize;
        if !id.valid() {
            return Err("Invalid saved artwork size.".into());
        }
        let mut body = Vec::new();
        let mut hash = Sha256::new();
        for index in 0..size.div_ceil(JOURNAL_PART) {
            let part = self.journal.load(self.blob_part(id, index)).map_err(|error| error.to_string())?.ok_or("A saved artwork chunk is missing.")?;
            if part.len() != JOURNAL_PART.min(size - body.len()) {
                return Err("A saved artwork chunk is invalid.".into());
            }
            if index == 0 {
                body.try_reserve_exact(size).map_err(|_| "Could not allocate the saved artwork.")?;
            }
            hash.update(&part);
            body.extend(part);
        }
        if <[u8; 32]>::from(hash.finalize()) != id.digest {
            return Err("Saved artwork failed verification.".into());
        }
        self.stored.borrow_mut().insert(id);
        Ok(Arc::new(body))
    }

    fn delete_parts(&self, id: BlobId, count: usize) {
        for index in 0..count {
            let _ = self.journal.delete(self.blob_part(id, index));
        }
    }

    fn write(&self, record: &[u8], blobs: &Blobs) -> Result<(), String> {
        let mut stored = self.stored.borrow_mut();
        // Write immutable sidecars before the record that references them.
        for (&id, body) in blobs {
            if !stored.contains(&id) {
                for (index, part) in body.chunks(JOURNAL_PART).enumerate() {
                    if let Err(error) = self.journal.store(self.blob_part(id, index), part) {
                        self.delete_parts(id, index + 1);
                        return Err(error.to_string());
                    }
                }
                stored.insert(id);
            }
        }
        self.journal.store(self.record, record).map_err(|error| error.to_string())?;
        // A declined queued edit no longer needs its encrypted artwork. Once the
        // metadata is committed, interrupted cleanup cannot damage the journal.
        stored.retain(|id| {
            if blobs.contains_key(id) {
                return true;
            }
            self.delete_parts(*id, (id.length as usize).div_ceil(JOURNAL_PART));
            false
        });
        Ok(())
    }
}
