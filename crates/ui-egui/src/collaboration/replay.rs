use std::collections::BTreeSet;

use designcraft_collab::{Operation, Replica, RoomProtocol};
use designcraft_doc::Document;

use super::{Commit, Message, edit_wire::WireEdit, encode_edit_for_protocol, seed_document::Fingerprints};

pub(super) struct RoomReplay {
    pub(super) document: Document,
    pub(super) replica: Replica,
    pub(super) history: Vec<(Commit, WireEdit)>,
    pub(super) inverses: Vec<Option<Vec<Operation>>>,
    pub(super) history_bytes: usize,
}

#[derive(Debug)]
pub(super) enum RestoreIssue {
    Diverged(&'static str),
    Other(String),
}

impl RestoreIssue {
    pub(super) fn into_message(self) -> String {
        match self {
            Self::Diverged(message) => message.into(),
            Self::Other(message) => message,
        }
    }
}

impl From<String> for RestoreIssue {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

#[cfg(test)]
pub(super) fn restore(seed: &Document, opened: &Document, confirmed: Vec<Commit>) -> Result<RoomReplay, String> {
    restore_for_room(seed, opened, confirmed, RoomProtocol::default())
}

pub(super) fn restore_for_room(seed: &Document, opened: &Document, confirmed: Vec<Commit>, protocol: RoomProtocol) -> Result<RoomReplay, String> {
    restore_cached_for_room(seed, opened, confirmed, &mut Fingerprints::for_room(protocol), protocol).map_err(RestoreIssue::into_message)
}

#[cfg(test)]
pub(super) fn restore_cached(
    seed: &Document,
    opened: &Document,
    confirmed: Vec<Commit>,
    cache: &mut Fingerprints,
) -> Result<RoomReplay, RestoreIssue> {
    restore_cached_for_room(seed, opened, confirmed, cache, RoomProtocol::default())
}

pub(super) fn restore_cached_for_room(
    seed: &Document,
    opened: &Document,
    confirmed: Vec<Commit>,
    cache: &mut Fingerprints,
    protocol: RoomProtocol,
) -> Result<RoomReplay, RestoreIssue> {
    if opened.created != seed.created {
        return Err(RestoreIssue::Diverged("The saved document is from a different room copy. Open the original file to resume."));
    }
    let opened_fingerprint = cache.canonical(opened)?;
    let opened_seed = cache.canonical(seed)? == opened_fingerprint;
    if confirmed.len() > protocol.history_limit() {
        return Err(RestoreIssue::Other("The local collaboration journal exceeds its history limit.".into()));
    }
    let mut document = seed.clone();
    let mut replica = Replica::for_room(seed, protocol);
    let mut history = Vec::with_capacity(confirmed.len());
    let mut inverses = Vec::with_capacity(confirmed.len());
    let mut history_bytes = 0;
    let mut artwork_size = super::seed_document::artwork_size(seed);
    let mut sequences = BTreeSet::new();
    for commit in confirmed {
        if commit.revision != history.len() as u64 + 1 {
            return Err(RestoreIssue::Other("The local collaboration journal has a missing revision.".into()));
        }
        if commit.proposal.sequence == 0 || !sequences.insert((commit.actor, commit.proposal.sequence)) {
            return Err(RestoreIssue::Other("The local collaboration journal has an invalid edit sequence.".into()));
        }
        let bytes = encode_edit_for_protocol(Message::Commit(commit.clone()), protocol)?;
        history_bytes += bytes.len();
        artwork_size += bytes.artwork_size;
        if history_bytes > protocol.history_byte_limit() || artwork_size > designcraft_collab::artwork::MAX_ARTWORK_BYTES as u64 {
            return Err(RestoreIssue::Other("The local collaboration journal exceeds its history limit.".into()));
        }
        super::history::verify_text_compensation(&commit, history.iter().map(|(original, _)| original), &inverses)?;
        let inverse = commit.apply_document(&mut replica, &mut document)?;
        inverses.push(inverse);
        history.push((commit, bytes));
    }
    if !opened_seed && cache.canonical(&document)? != opened_fingerprint {
        // A saved file usually matches one endpoint. Recheck older saved revisions
        // on demand so a current book is not serialized after every replayed edit.
        let mut earlier = seed.clone();
        let mut earlier_replica = Replica::for_room(seed, protocol);
        let mut found = false;
        for (commit, _) in history.iter().take(history.len().saturating_sub(1)) {
            commit.apply_document(&mut earlier_replica, &mut earlier)?;
            if cache.canonical(&earlier)? == opened_fingerprint {
                found = true;
                break;
            }
        }
        if !found {
            return Err(RestoreIssue::Diverged("The saved document has changes outside this room. Open a room copy to resume."));
        }
    }
    // Save can update local file metadata without changing the room revision.
    document.modified = opened.modified;
    document.font_scope = opened.font_scope;
    Ok(RoomReplay { document, replica, history, inverses, history_bytes })
}
