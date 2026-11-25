use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use designcraft_collab_core::{ActorId, Identity};
use designcraft_doc::Document;
use serde::{Deserialize, Deserializer, Serialize, de};
use std::{collections::VecDeque, fmt, path::Path, sync::Arc};

use super::{Commit, MAX_QUEUED_STORY_EDITS, Proposal, attachments::Blobs, seed_document};
use designcraft_collab::{RoomProtocol, artwork::BlobId};

const JOURNAL_VERSION: u16 = 1;
#[cfg(target_os = "macos")]
const SEGMENTED_JOURNAL_VERSION: u16 = 2;
#[cfg(target_os = "macos")]
const HISTORY_SEGMENT_EDITS: usize = 64;
#[cfg(target_os = "macos")]
const HISTORY_SEGMENT_BYTES: usize = 512 * 1024;
#[cfg(all(test, target_os = "macos"))]
pub(super) const JOURNAL_DIRECTORY: &str = journal_directory(RoomProtocol::CURRENT);

#[cfg(target_os = "macos")]
// Keep the development path stable so existing protocol-16 journals can migrate in place.
const fn journal_directory(protocol: RoomProtocol) -> &'static str {
    if protocol.is_automerge() { "Collaboration-Protocol16" } else { "Collaboration" }
}

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
    pending: PendingRecord<'a>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum PendingRecord<'a> {
    Legacy(Option<&'a Proposal>),
    Queued(&'a VecDeque<Proposal>),
}

impl<'a> PendingRecord<'a> {
    fn new(protocol: RoomProtocol, pending: &'a VecDeque<Proposal>) -> Self {
        if protocol.is_automerge() { Self::Queued(pending) } else { Self::Legacy(pending.front()) }
    }
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
pub(super) struct SavedRoom<P = VecDeque<Proposal>> {
    version: u16,
    identity: [u8; 32],
    seed: SavedSeed,
    pub(super) next_sequence: u64,
    pub(super) confirmed: Vec<Commit>,
    #[serde(default)]
    pub(super) pending: P,
}

impl<P> SavedRoom<P> {
    fn map_pending(self, map: impl FnOnce(P) -> VecDeque<Proposal>) -> SavedRoom {
        SavedRoom {
            version: self.version,
            identity: self.identity,
            seed: self.seed,
            next_sequence: self.next_sequence,
            confirmed: self.confirmed,
            pending: map(self.pending),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(transparent)]
struct QueuedPending(#[serde(deserialize_with = "deserialize_pending")] VecDeque<Proposal>);

fn read_saved_room(bytes: &[u8], protocol: RoomProtocol) -> Result<SavedRoom, serde_json::Error> {
    if protocol.is_automerge() {
        serde_json::from_slice::<SavedRoom<QueuedPending>>(bytes).map(|saved| saved.map_pending(|pending| pending.0))
    } else {
        serde_json::from_slice::<SavedRoom<Option<Proposal>>>(bytes).map(|saved| saved.map_pending(|pending| pending.into_iter().collect()))
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HistoryTail {
    segments: usize,
    archived_edits: usize,
    commits: Vec<Commit>,
    bytes: usize,
}

#[cfg(target_os = "macos")]
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HistoryHead {
    tail: HistoryTail,
    next_sequence: u64,
    #[serde(deserialize_with = "deserialize_pending")]
    pending: VecDeque<Proposal>,
}

#[cfg(target_os = "macos")]
#[derive(Serialize)]
struct StoryOutboxRecord<'a> {
    confirmed_revision: u64,
    next_sequence: u64,
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
            validate_story_outbox(RoomProtocol::Automerge, outbox.confirmed_revision, &outbox.pending, outbox.next_sequence)?;
            self.pending = outbox.pending;
            self.next_sequence = outbox.next_sequence;
        }
        Ok(())
    }
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
            let mut proposals = VecDeque::new();
            while proposals.len() < MAX_QUEUED_STORY_EDITS {
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

fn validate_pending(protocol: RoomProtocol, pending: &VecDeque<Proposal>, next_sequence: u64) -> Result<(), String> {
    let max_pending = if protocol.is_automerge() { MAX_QUEUED_STORY_EDITS } else { 1 };
    if pending.len() > max_pending {
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

fn validate_story_outbox(protocol: RoomProtocol, confirmed_revision: u64, pending: &VecDeque<Proposal>, next_sequence: u64) -> Result<(), String> {
    if !protocol.is_automerge() {
        return Err("Queued story recovery needs a compatible room version.".into());
    }
    validate_pending(protocol, pending, next_sequence)?;
    if pending.is_empty() || confirmed_revision > protocol.history_limit() as u64 {
        return Err("The queued story recovery record is invalid.".into());
    }
    for proposal in pending {
        // A pure story proposal cannot contain an artwork operation or an undo request.
        proposal.validate_story_transaction_for_protocol(protocol)?;
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

    fn saved_record(protocol: RoomProtocol, pending: Value) -> Result<SavedRoom, serde_json::Error> {
        let value = json!({
            "version": JOURNAL_VERSION, "identity": vec![0u8; 32], "seed": "",
            "next_sequence": 100, "confirmed": [], "pending": pending,
        });
        read_saved_room(&serde_json::to_vec(&value)?, protocol)
    }

    fn proposal(sequence: u64) -> Value {
        json!({"sequence": sequence, "base": 0, "edits": []})
    }

    fn encode(protocol: RoomProtocol, pending: &VecDeque<Proposal>) -> Value {
        serde_json::to_value(Record {
            version: JOURNAL_VERSION,
            identity: &[0; 32],
            seed: "",
            next_sequence: 100,
            confirmed: Vec::new(),
            pending: PendingRecord::new(protocol, pending),
        })
        .unwrap()
    }

    #[test]
    fn legacy_and_automerge_journals_write_their_own_pending_shape() {
        for (protocol, empty_value, single_value) in
            [(RoomProtocol::Legacy, Value::Null, proposal(7)), (RoomProtocol::Automerge, json!([]), json!([proposal(7)]))]
        {
            let empty = saved_record(protocol, Value::Null).unwrap();
            let single = saved_record(protocol, proposal(7)).unwrap();
            assert!(empty.pending.is_empty());
            assert_eq!(single.pending.front().unwrap().sequence, 7);
            assert_eq!(encode(protocol, &empty.pending)["pending"], empty_value);
            assert_eq!(encode(protocol, &single.pending)["pending"], single_value);
        }
    }

    #[test]
    fn pending_edits_retain_order_and_always_obey_the_protocol_bound() {
        let protocol = RoomProtocol::Automerge;
        let encoded_queue = json!([proposal(7), proposal(9)]);
        let queue = saved_record(protocol, encoded_queue.clone()).unwrap();
        assert_eq!(encode(protocol, &queue.pending)["pending"], encoded_queue);
        assert!(saved_record(RoomProtocol::Legacy, encoded_queue).is_err());
        assert!(validate_pending(RoomProtocol::Legacy, &queue.pending, queue.next_sequence).is_err());
        let limit: Vec<_> = (1..=MAX_QUEUED_STORY_EDITS as u64).map(proposal).collect();
        let at_limit = saved_record(protocol, json!(limit)).unwrap();
        assert_eq!(at_limit.pending.len(), MAX_QUEUED_STORY_EDITS);
        let over_limit: Vec<_> = (1..=MAX_QUEUED_STORY_EDITS as u64 + 1).map(proposal).collect();
        assert!(saved_record(protocol, json!(over_limit)).is_err());
        let out_of_order = saved_record(protocol, json!([proposal(9), proposal(7)])).unwrap();
        assert!(validate_pending(protocol, &out_of_order.pending, out_of_order.next_sequence).is_err());
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
        let mut saved = saved_record(RoomProtocol::Automerge, Value::Null).unwrap();
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
        let mut saved = saved_record(RoomProtocol::Automerge, story_proposal(7, 0)).unwrap();
        for pending in [json!([proposal(100)]), json!([story_proposal(100, 1)]), json!([story_proposal(101, 0)])] {
            assert!(saved.restore_story_outbox(shadow(0, 101, pending)).is_err());
            assert_eq!(saved.next_sequence, 100);
            assert_eq!(saved.pending.front().unwrap().sequence, 7);
        }
    }
}

/// Exercise both on-disk formats under the existing disposable-Keychain restart fixture.
#[cfg(all(test, target_os = "macos", feature = "automerge-rooms"))]
pub(super) fn verify_legacy_room_protocol_fallback(root: &Path) -> Result<(), String> {
    let file = tempfile::NamedTempFile::new_in(root).map_err(|error| error.to_string())?;
    let path = file.path().to_str();
    let directory = root.join("Protocol selection");
    let recovery = directory.join("Recovery");
    let seed = Document::new(&designcraft_doc::build::NewDocument::default());
    let open = |protocol| Storage::open_for_protocol(Some(&recovery), path, &seed, None, OpenRoom::Active, protocol);

    let (legacy, _) = open(Some(RoomProtocol::Legacy))?;
    legacy.write(std::iter::empty(), &VecDeque::new(), 1)?;
    let legacy_actor = legacy.identity().actor();
    drop(legacy);

    let (selected, saved) = open(None)?;
    assert_eq!(selected.protocol, RoomProtocol::Legacy);
    assert_eq!(selected.identity().actor(), legacy_actor);
    assert!(saved.pending.is_empty());
    let unused = directory.join(journal_directory(RoomProtocol::Automerge));
    assert!(matches!(designcraft_engine::io::metadata(unused), Err(error) if error.kind() == std::io::ErrorKind::NotFound));
    drop(selected);

    let (automerge, _) = open(Some(RoomProtocol::Automerge))?;
    automerge.write(std::iter::empty(), &VecDeque::new(), 1)?;
    let automerge_actor = automerge.identity().actor();
    assert_ne!(automerge_actor, legacy_actor);
    drop(automerge);

    let (selected, _) = open(None)?;
    assert_eq!(selected.protocol, RoomProtocol::Automerge);
    assert_eq!(selected.identity().actor(), automerge_actor);
    let (legacy, _) = open(Some(RoomProtocol::Legacy))?;
    assert_eq!(legacy.protocol, RoomProtocol::Legacy);
    assert_eq!(legacy.identity().actor(), legacy_actor);
    Ok(())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SavedSeed {
    Archive(String),
    Model(Box<Document>),
}

pub(super) struct Storage {
    identity: [u8; 32],
    pub(super) protocol: RoomProtocol,
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
    confirmed_blobs: std::cell::RefCell<std::collections::BTreeSet<BlobId>>,
    pending_blobs: std::cell::RefCell<std::collections::BTreeSet<BlobId>>,
    history: std::cell::RefCell<Option<HistoryTail>>,
    hosts: Option<HostRooms>,
}

#[cfg(target_os = "macos")]
struct HostRooms {
    index: RecordId,
    records: Vec<[u8; 32]>,
    staged: bool,
}

impl Storage {
    pub(super) fn ephemeral_for_protocol(document: Document, identity: [u8; 32], protocol: RoomProtocol) -> Result<Self, String> {
        if !protocol.is_supported() {
            return Err("This room uses a different collaboration protocol.".into());
        }
        let (archive, seed_blobs) = seed_document::bundle_for_room(&document, protocol)?;
        Ok(Self {
            identity,
            protocol,
            seed_blobs,
            seed: Arc::new(document),
            encoded_archive: STANDARD_NO_PAD.encode(&archive),
            archive,
            #[cfg(target_os = "macos")]
            durable: None,
        })
    }

    pub(super) fn ephemeral_preview_for_protocol(document: Document, identity: [u8; 32], protocol: RoomProtocol) -> Self {
        Self {
            identity,
            protocol,
            seed_blobs: Blobs::new(),
            seed: Arc::new(document),
            encoded_archive: String::new(),
            archive: Vec::new(),
            #[cfg(target_os = "macos")]
            durable: None,
        }
    }

    #[cfg(all(test, target_os = "macos", feature = "automerge-rooms"))]
    pub(super) fn open(
        recovery_dir: Option<&Path>,
        document_path: Option<&str>,
        document: &Document,
        guest_of: Option<ActorId>,
        room: OpenRoom,
    ) -> Result<(Self, SavedRoom), String> {
        Self::open_for_protocol(recovery_dir, document_path, document, guest_of, room, None)
    }

    pub(super) fn open_for_protocol(
        recovery_dir: Option<&Path>,
        document_path: Option<&str>,
        document: &Document,
        guest_of: Option<ActorId>,
        room: OpenRoom,
        requested: Option<RoomProtocol>,
    ) -> Result<(Self, SavedRoom), String> {
        let preferred = requested.unwrap_or_default();
        if !preferred.is_supported() {
            return Err("The local collaboration journal needs a compatible app version.".into());
        }
        #[cfg(target_os = "macos")]
        let (protocol, mut durable) = Durable::open(recovery_dir, document_path, guest_of, requested, preferred)?;
        #[cfg(not(target_os = "macos"))]
        let protocol = {
            let _ = (recovery_dir, document_path, guest_of);
            preferred
        };

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
            let saved = read_saved_room(&bytes, protocol).map_err(|_| "The local collaboration journal could not be read.")?;
            #[cfg(target_os = "macos")]
            let saved = if protocol.is_automerge() && saved.version == SEGMENTED_JOURNAL_VERSION {
                durable.as_ref().ok_or("The room recovery directory is missing.")?.restore_history(saved, protocol)?
            } else {
                saved
            };
            if saved.version != JOURNAL_VERSION || saved.next_sequence == 0 || saved.confirmed.len() > protocol.history_limit() {
                return Err("The local collaboration journal needs a compatible app version.".into());
            }
            validate_pending(protocol, &saved.pending, saved.next_sequence)?;
            #[cfg(target_os = "macos")]
            let saved = if protocol.is_automerge()
                && let Some(durable) = durable.as_ref().filter(|durable| durable.record.role == Role::Guest)
            {
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
                let (archive, blobs) = seed_document::bundle_for_room(seed, protocol)?;
                ((**seed).clone(), archive, blobs)
            }
            SavedSeed::Archive(encoded) => {
                let archive = STANDARD_NO_PAD.decode(encoded).map_err(|_| "The saved seed archive is invalid.")?;
                let mut blobs = Blobs::new();
                let doc = seed_document::restore_bundle_for_room(
                    &archive,
                    |id| {
                        let body = read(id)?;
                        blobs.insert(id, body.clone());
                        Ok(body)
                    },
                    protocol,
                )?;
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
        #[cfg(target_os = "macos")]
        if protocol.is_automerge()
            && let Some(durable) = &durable
        {
            durable
                .confirmed_blobs
                .borrow_mut()
                .extend(seed_blobs.keys().copied().chain(saved.confirmed.iter().flat_map(|c| c.proposal.blob_refs())));
            durable.pending_blobs.borrow_mut().extend(saved.pending.iter().flat_map(Proposal::blob_refs));
        }
        let storage = Self {
            identity: saved.identity,
            protocol,
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
        let durable = Durable::new(root, room_record(path, Some(host))?, self.protocol)?;
        if durable.journal.load(durable.record).map_err(|error| error.to_string())?.is_some() {
            return Err("This path already has a saved room. Keep this copy under a different name for recovery.".into());
        }
        if self.archive.is_empty() {
            let (archive, blobs) = seed_document::bundle_for_room(&self.seed, self.protocol)?;
            self.encoded_archive = STANDARD_NO_PAD.encode(&archive);
            self.archive = archive;
            self.seed_blobs = blobs;
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
        validate_story_outbox(self.protocol, confirmed_revision, pending, next_sequence)?;
        #[cfg(target_os = "macos")]
        if let Some(durable) = &self.durable {
            if durable.record.role != Role::Guest {
                return Err("Only a guest can keep a queued story recovery record.".into());
            }
            let bytes = encode_record(&StoryOutboxRecord { confirmed_revision, pending, next_sequence })?;
            // The next ACK must also fit the encrypted history tail in the same record.
            if bytes.len() > MAX_RECORD_BYTES - super::seed::MAX_SEED_BYTES - 1024 {
                return Err("Wait for your queued story edits to sync before pasting more text.".into());
            }
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
        validate_pending(self.protocol, pending, next_sequence)?;
        if !self.is_durable() {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        if self.protocol.is_automerge()
            && let Some(durable) = &self.durable
        {
            let existing = durable.history.borrow().clone();
            if let Some(tail) = existing {
                durable.write_proposal_blobs(pending.iter())?;
                durable.write_history_head(&self.identity, tail, pending, next_sequence)?;
                durable.retire_pending_blobs(pending);
                return Ok(());
            }
            let mut tail = HistoryTail::default();
            durable.store_blobs(&self.seed_blobs)?;
            let mut confirmed_blobs: std::collections::BTreeSet<_> = self.seed_blobs.keys().copied().collect();
            for commit in confirmed {
                durable.write_proposal_blobs(std::iter::once(&commit.proposal))?;
                confirmed_blobs.extend(commit.proposal.blob_refs());
                let bytes = encode_record(commit)?.len();
                durable.stage_commit(&self.identity, &mut tail, commit, bytes)?;
            }
            let base = Record {
                version: SEGMENTED_JOURNAL_VERSION,
                identity: &self.identity,
                seed: &self.encoded_archive,
                next_sequence: 1,
                confirmed: Vec::new(),
                pending: PendingRecord::Queued(&VecDeque::new()),
            };
            let base = encode_record(&base)?;
            durable.write_proposal_blobs(pending.iter())?;
            durable.write_history_head(&self.identity, tail, pending, next_sequence)?;
            // Publishing the base last keeps a legacy journal recoverable if migration is interrupted.
            if let Err(error) = durable.journal.store(durable.record, &base) {
                durable.history.replace(None);
                return Err(error.to_string());
            }
            durable.confirmed_blobs.replace(confirmed_blobs);
            durable.retire_pending_blobs(pending);
            return Ok(());
        }
        self.write_legacy(confirmed, pending, next_sequence)
    }

    pub(super) fn write_legacy<'a>(
        &self,
        confirmed: impl Iterator<Item = &'a Commit>,
        pending: &VecDeque<Proposal>,
        next_sequence: u64,
    ) -> Result<(), String> {
        validate_pending(self.protocol, pending, next_sequence)?;
        let record = Record {
            version: JOURNAL_VERSION,
            identity: &self.identity,
            seed: &self.encoded_archive,
            next_sequence,
            confirmed: confirmed.collect(),
            pending: PendingRecord::new(self.protocol, pending),
        };
        #[cfg(target_os = "macos")]
        let bytes = encode_record(&record)?;
        #[cfg(not(target_os = "macos"))]
        let bytes = serde_json::to_vec(&record).map_err(|_| "The local collaboration journal could not be encoded.")?;
        #[cfg(target_os = "macos")]
        if let Some(durable) = &self.durable {
            let mut blobs = self.seed_blobs.clone();
            for proposal in record.confirmed.iter().map(|c| &c.proposal).chain(pending) {
                blobs.extend(proposal.blobs()?);
            }
            super::attachments::total(&blobs)?;
            durable.write(&bytes, &blobs)?;
        }
        #[cfg(not(target_os = "macos"))]
        let _ = bytes;
        Ok(())
    }

    pub(super) fn append_confirmed<'a>(
        &self,
        confirmed: impl Iterator<Item = &'a Commit>,
        commit: &'a Commit,
        wire_bytes: usize,
        pending: &VecDeque<Proposal>,
        next_sequence: u64,
    ) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        if self.protocol.is_automerge()
            && let Some(durable) = &self.durable
        {
            let existing = durable.history.borrow().clone();
            if let Some(mut tail) = existing {
                validate_pending(self.protocol, pending, next_sequence)?;
                durable.write_proposal_blobs(std::iter::once(&commit.proposal).chain(pending))?;
                durable.stage_commit(&self.identity, &mut tail, commit, wire_bytes)?;
                durable.write_history_head(&self.identity, tail, pending, next_sequence)?;
                durable.confirmed_blobs.borrow_mut().extend(commit.proposal.blob_refs());
                durable.retire_pending_blobs(pending);
                return Ok(());
            }
        }
        let _ = wire_bytes;
        self.write(confirmed.chain(std::iter::once(commit)), pending, next_sequence)
    }
}

#[cfg(target_os = "macos")]
impl Durable {
    fn open(
        recovery_dir: Option<&Path>,
        document_path: Option<&str>,
        guest_of: Option<ActorId>,
        requested: Option<RoomProtocol>,
        preferred: RoomProtocol,
    ) -> Result<(RoomProtocol, Option<Self>), String> {
        let (Some(root), Some(path)) = (recovery_dir.and_then(Path::parent), document_path) else { return Ok((preferred, None)) };
        let record = room_record(path, guest_of)?;
        let protocol = if requested.is_none() && guest_of.is_none() && preferred.is_automerge() {
            Self::existing_host_protocol(root, record, preferred)?
        } else {
            preferred
        };
        Ok((protocol, Some(Self::new(root, record, protocol)?)))
    }

    fn history_id(&self, identity: &[u8; 32], segment: u64) -> RecordId {
        let mut hash = Sha256::new();
        hash.update(b"Collate encrypted confirmed history v2");
        hash.update(self.record.room);
        hash.update(identity);
        hash.update(segment.to_be_bytes());
        RecordId { room: hash.finalize().into(), role: self.record.role }
    }

    fn restore_history(&self, mut saved: SavedRoom, protocol: RoomProtocol) -> Result<SavedRoom, String> {
        const INVALID: &str = "The encrypted room history is incomplete or invalid.";
        if saved.next_sequence != 1 || !saved.pending.is_empty() || !saved.confirmed.is_empty() {
            return Err(INVALID.into());
        }
        let bytes = self.journal.load(self.history_id(&saved.identity, 0)).map_err(|error| error.to_string())?.ok_or(INVALID)?;
        let HistoryHead { tail, pending, next_sequence } = serde_json::from_slice(&bytes).map_err(|_| INVALID)?;
        let total = tail.archived_edits.checked_add(tail.commits.len()).filter(|&edits| edits <= protocol.history_limit()).ok_or(INVALID)?;
        if tail.segments > tail.archived_edits || tail.commits.len() > HISTORY_SEGMENT_EDITS || tail.bytes > protocol.history_byte_limit() {
            return Err(INVALID.into());
        }
        let mut encoded_bytes = 0usize;
        for ordinal in 1..=tail.segments {
            let bytes = self.journal.load(self.history_id(&saved.identity, ordinal as u64)).map_err(|error| error.to_string())?.ok_or(INVALID)?;
            encoded_bytes = encoded_bytes.saturating_add(bytes.len());
            if encoded_bytes > protocol.history_byte_limit() {
                return Err(INVALID.into());
            }
            let commits: Vec<Commit> = serde_json::from_slice(&bytes).map_err(|_| INVALID)?;
            if commits.is_empty() || commits.len() > HISTORY_SEGMENT_EDITS || commits.len() > tail.archived_edits - saved.confirmed.len() {
                return Err(INVALID.into());
            }
            saved.confirmed.extend(commits);
        }
        saved.confirmed.extend(tail.commits.iter().cloned());
        if saved.confirmed.len() != total || saved.confirmed.iter().enumerate().any(|(index, commit)| commit.revision != index as u64 + 1) {
            return Err(INVALID.into());
        }
        saved.version = JOURNAL_VERSION;
        saved.pending = pending;
        saved.next_sequence = next_sequence;
        self.history.replace(Some(tail));
        Ok(saved)
    }

    fn stage_commit(&self, identity: &[u8; 32], tail: &mut HistoryTail, commit: &Commit, bytes: usize) -> Result<(), String> {
        if commit.revision != (tail.archived_edits + tail.commits.len()) as u64 + 1 {
            return Err("The encrypted room history has an unexpected revision.".into());
        }
        if !tail.commits.is_empty() && (tail.commits.len() >= HISTORY_SEGMENT_EDITS || tail.bytes.saturating_add(bytes) > HISTORY_SEGMENT_BYTES) {
            let ordinal = tail.segments + 1;
            let sealed = encode_record(&tail.commits)?;
            self.journal.store(self.history_id(identity, ordinal as u64), &sealed).map_err(|error| error.to_string())?;
            tail.segments = ordinal;
            tail.archived_edits += tail.commits.len();
            tail.commits.clear();
            tail.bytes = 0;
        }
        tail.commits.push(commit.clone());
        tail.bytes += bytes;
        Ok(())
    }

    fn write_history_head(&self, identity: &[u8; 32], tail: HistoryTail, pending: &VecDeque<Proposal>, next_sequence: u64) -> Result<(), String> {
        let head = HistoryHead { tail, next_sequence, pending: pending.clone() };
        self.journal.store(self.history_id(identity, 0), &encode_record(&head)?).map_err(|error| error.to_string())?;
        self.history.replace(Some(head.tail));
        Ok(())
    }

    fn existing_host_protocol(root: &Path, path_record: RecordId, preferred: RoomProtocol) -> Result<RoomProtocol, String> {
        let index = host_index(path_record);
        for protocol in [preferred, RoomProtocol::Legacy] {
            // Discovery must not create the unused journal or hide a corrupt index
            // by falling back to another room on the same document path.
            let journal = Journal::new(root.join(journal_directory(protocol)));
            if journal.load(index).map_err(|error| error.to_string())?.is_some()
                || journal.load(path_record).map_err(|error| error.to_string())?.is_some()
            {
                return Ok(protocol);
            }
        }
        Ok(preferred)
    }

    fn new(root: &Path, path_record: RecordId, protocol: RoomProtocol) -> Result<Self, String> {
        let journal = Journal::new(root.join(journal_directory(protocol)));
        // All host versions of the path share the existing writer lease. The
        // pointer and replacement can only be changed while that lease is held.
        let lease = journal.lock(path_record).map_err(|error| error.to_string())?;
        let mut record = path_record;
        let hosts = if path_record.role == Role::Host {
            let index = host_index(path_record);
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
        Ok(Self {
            journal,
            record,
            _lease: lease,
            stored: Default::default(),
            confirmed_blobs: Default::default(),
            pending_blobs: Default::default(),
            history: Default::default(),
            hosts,
        })
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
fn room_record(path: &str, guest_of: Option<ActorId>) -> Result<RecordId, String> {
    let canonical = designcraft_engine::io::canonicalize(path).map_err(|_| "Could not find the saved document for collaboration.")?;
    let mut hash = Sha256::new();
    hash.update(canonical.as_os_str().as_encoded_bytes());
    if let Some(host) = guest_of {
        hash.update(host.0);
    }
    let role = if guest_of.is_some() { Role::Guest } else { Role::Host };
    Ok(RecordId { room: hash.finalize().into(), role })
}

#[cfg(target_os = "macos")]
fn host_index(path_record: RecordId) -> RecordId {
    host_record(b"Collate host room index v1", path_record.room, &[])
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

    fn write_proposal_blobs<'a>(&self, proposals: impl Iterator<Item = &'a Proposal>) -> Result<(), String> {
        for proposal in proposals {
            self.store_blobs(&proposal.blobs()?)?;
        }
        Ok(())
    }

    fn retire_pending_blobs(&self, pending: &VecDeque<Proposal>) {
        let current = pending.iter().flat_map(Proposal::blob_refs).collect();
        let previous = self.pending_blobs.replace(current);
        let pending = self.pending_blobs.borrow();
        let confirmed = self.confirmed_blobs.borrow();
        let mut stored = self.stored.borrow_mut();
        // Only a successful head write can make previous pending artwork disposable.
        for id in previous.difference(&pending).filter(|id| !confirmed.contains(id)) {
            stored.remove(id);
            self.delete_parts(*id, (id.length as usize).div_ceil(JOURNAL_PART));
        }
    }

    fn store_blobs(&self, blobs: &Blobs) -> Result<(), String> {
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
        Ok(())
    }

    fn write(&self, record: &[u8], blobs: &Blobs) -> Result<(), String> {
        self.store_blobs(blobs)?;
        self.journal.store(self.record, record).map_err(|error| error.to_string())?;
        // A declined queued edit no longer needs its encrypted artwork. Once the
        // metadata is committed, interrupted cleanup cannot damage the journal.
        self.stored.borrow_mut().retain(|id| {
            if blobs.contains_key(id) {
                return true;
            }
            self.delete_parts(*id, (id.length as usize).div_ceil(JOURNAL_PART));
            false
        });
        Ok(())
    }
}
