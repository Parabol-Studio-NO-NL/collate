//! Authenticated Automerge transactions in the experimental room protocol.

use designcraft_collab::{Error, Operation, Replica, automerge_stories::StoryTransaction, text::TextSplice};
use designcraft_collab_core::ActorId;
use designcraft_doc::{Document, StoryId, TextSel};
use designcraft_engine::{DeferredEdit, DocState};

use super::{Commit, Proposal, operation_story};

pub(super) struct PendingSelection {
    pub(super) sequence: u64,
    selection: TextSel,
    anchor: Vec<u8>,
    focus: Vec<u8>,
}

pub(super) struct Captured {
    pub(super) transaction: StoryTransaction,
    pub(super) selection: Option<PendingSelection>,
}

impl PendingSelection {
    fn resolve(&self, updated: &Replica) -> Option<TextSel> {
        resolve(updated, self.selection, &self.anchor, &self.focus)
    }
}

pub(super) fn capture(replica: &Replica, actor: ActorId, edit: &DeferredEdit, sequence: u64) -> Result<Option<Captured>, String> {
    let mut draft = replica.clone();
    let hint = edit.before_selection.text.zip(edit.after_selection.text).and_then(|(before, after)| splice(before, after, &edit.before, &edit.after));
    let transaction = match draft.capture_automerge_text(&edit.before, &edit.after, &actor.0, hint) {
        Ok(Some(transaction)) => transaction,
        Ok(None) | Err(Error::UnsupportedEdit) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let selection = edit.after_selection.text.and_then(|selection| {
        let anchor = draft.automerge_cursor(selection.story, selection.anchor)?;
        let focus = draft.automerge_cursor(selection.story, selection.focus)?;
        Some(PendingSelection { sequence, selection, anchor, focus })
    });
    Ok(Some(Captured { transaction, selection }))
}

fn splice(before: TextSel, after: TextSel, source: &Document, destination: &Document) -> Option<(StoryId, TextSplice)> {
    if before.story != after.story || before.cell.is_some() || after.cell.is_some() || !after.is_caret() {
        return None;
    }
    let old_len = source.story(before.story)?.text.len();
    let new_len = destination.story(after.story)?.text.len();
    let old = before.range();
    let hint = if before.is_caret() && new_len < old_len {
        TextSplice { old: after.focus..after.focus.checked_add(old_len - new_len)?, new: after.focus..after.focus }
    } else {
        TextSplice { new: old.start..after.focus, old }
    };
    Some((before.story, hint))
}

pub(super) fn selection_after(
    state: &DocState,
    before: &Replica,
    after: &Replica,
    transaction: &StoryTransaction,
    local: Option<&PendingSelection>,
) -> Option<TextSel> {
    if let Some(local) = local
        && state
            .selection
            .text
            .is_some_and(|current| current.story == local.selection.story && current.frame == local.selection.frame && current.cell.is_none())
    {
        return local.resolve(after);
    }
    let selection = state.selection.text?;
    if selection.cell.is_some() || !transaction.changes.iter().any(|change| change.story == selection.story) {
        return None;
    }
    let [anchor, focus] = before.automerge_anchors(&state.doc, selection.story, [selection.anchor, selection.focus])?;
    resolve(after, selection, &anchor, &focus)
}

fn resolve(updated: &Replica, mut selection: TextSel, anchor: &[u8], focus: &[u8]) -> Option<TextSel> {
    selection.anchor = updated.resolve_automerge_cursor(selection.story, anchor)?;
    selection.focus = updated.resolve_automerge_cursor(selection.story, focus)?;
    Some(selection)
}

impl Proposal {
    pub(super) fn validate_story_transaction(&self) -> Result<(), String> {
        if self.story_transaction.is_some() {
            if !cfg!(feature = "automerge-rooms") {
                return Err("This room does not accept Automerge story edits.".into());
            }
            if !self.edits.is_empty() || self.history_action.is_some() {
                return Err("An Automerge story transaction cannot contain room commands or undo requests.".into());
            }
        }
        Ok(())
    }

    pub(super) fn affected_stories(&self) -> impl Iterator<Item = StoryId> + '_ {
        self.story_transaction
            .iter()
            .flat_map(|transaction| transaction.changes.iter().map(|change| change.story))
            .chain(self.edits.iter().filter_map(operation_story))
    }

    pub(super) fn is_text_only(&self) -> bool {
        self.story_transaction.is_some() || self.edits.iter().all(|operation| operation_story(operation).is_some())
    }

    pub(super) fn conflicts_with(&self, other: &Self) -> bool {
        if let Some(transaction) = &self.story_transaction {
            return other.edits.iter().any(|edit| conflicts_with_story(transaction, edit));
        }
        if let Some(transaction) = &other.story_transaction {
            return self.edits.iter().any(|edit| conflicts_with_story(transaction, edit));
        }
        self.edits.iter().any(|edit| other.edits.iter().any(|previous| edit.conflicts(previous)))
    }
}

fn conflicts_with_story(transaction: &StoryTransaction, edit: &Operation) -> bool {
    match edit {
        Operation::Page { .. } | Operation::Layer { .. } => true,
        Operation::DeleteText { story, .. } | Operation::Text { story, .. } | Operation::SetTextBatchActive { story, .. } => {
            transaction.changes.iter().any(|change| change.story == *story)
        }
        _ => false,
    }
}

impl Commit {
    pub(super) fn apply_document(&self, replica: &mut Replica, document: &mut Document) -> Result<Option<Vec<Operation>>, String> {
        self.proposal.validate_story_transaction()?;
        if let Some(transaction) = &self.proposal.story_transaction {
            if self.correction.is_some() {
                return Err("An Automerge story edit cannot be applied as an undo or redo.".into());
            }
            replica.apply_automerge_text(document, &self.actor.0, transaction).map_err(|error| error.to_string())?;
            return Ok(None);
        }
        let edits = &self.proposal.edits;
        let inverse = replica.inverse_patch_batch(document, edits).map_err(|error| error.to_string())?;
        replica.apply(document, edits).map_err(|error| error.to_string())?;
        Ok(inverse)
    }
}
