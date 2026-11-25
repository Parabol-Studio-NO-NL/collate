use designcraft_collab::{Operation, Replica};
use designcraft_doc::Document;

use super::{Commit, MAX_HISTORY_BYTES, Message, edit_wire::WireEdit, encode_edit, seed_document::Fingerprints};

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

pub(super) fn restore(seed: &Document, opened: &Document, confirmed: Vec<Commit>) -> Result<RoomReplay, String> {
    restore_cached(seed, opened, confirmed, &mut Fingerprints::default()).map_err(RestoreIssue::into_message)
}

pub(super) fn restore_cached(
    seed: &Document,
    opened: &Document,
    confirmed: Vec<Commit>,
    cache: &mut Fingerprints,
) -> Result<RoomReplay, RestoreIssue> {
    if opened.created != seed.created {
        return Err(RestoreIssue::Diverged("The saved document is from a different room copy. Open the original file to resume."));
    }
    let opened_fingerprint = cache.canonical(opened)?;
    let mut found_opened = cache.canonical(seed)? == opened_fingerprint;
    let mut document = seed.clone();
    let mut replica = Replica::new(seed);
    let mut history = Vec::with_capacity(confirmed.len());
    let mut inverses = Vec::with_capacity(confirmed.len());
    let mut history_bytes = 0;
    let mut artwork_size = super::seed_document::artwork_size(seed);
    for commit in confirmed {
        if commit.revision != history.len() as u64 + 1 {
            return Err(RestoreIssue::Other("The local collaboration journal has a missing revision.".into()));
        }
        let bytes = encode_edit(Message::Commit(commit.clone()))?;
        history_bytes += bytes.len();
        artwork_size += bytes.artwork_size;
        if history_bytes > MAX_HISTORY_BYTES || artwork_size > designcraft_collab::artwork::MAX_ARTWORK_BYTES as u64 {
            return Err(RestoreIssue::Other("The local collaboration journal exceeds its history limit.".into()));
        }
        super::history::verify_text_compensation(&commit, history.iter().map(|(original, _)| original), &inverses)?;
        let inverse = commit.apply_document(&mut replica, &mut document)?;
        inverses.push(inverse);
        if !found_opened {
            found_opened = cache.canonical(&document)? == opened_fingerprint;
        }
        history.push((commit, bytes));
    }
    if !found_opened {
        return Err(RestoreIssue::Diverged("The saved document has changes outside this room. Open a room copy to resume."));
    }
    // Save can update local file metadata without changing the room revision.
    document.modified = opened.modified;
    document.font_scope = opened.font_scope;
    Ok(RoomReplay { document, replica, history, inverses, history_bytes })
}
