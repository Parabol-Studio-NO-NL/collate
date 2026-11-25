use designcraft_collab::Operation;
use designcraft_collab_core::ActorId;
use serde::{Deserialize, Serialize};

use super::{Commit, Proposal, Room};

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum HistoryAction {
    Undo,
    Redo,
}

impl HistoryAction {
    pub(super) fn from_command(command: &str) -> Option<Self> {
        match command {
            "edit.undo" => Some(Self::Undo),
            "edit.redo" => Some(Self::Redo),
            _ => None,
        }
    }
}

struct OwnEdit {
    original: u64,
    effective: u64,
    undone: bool,
}

impl Room {
    pub(super) fn resolve_history(&self, actor: ActorId, proposal: &Proposal) -> Result<Option<(u64, Vec<Operation>)>, String> {
        let Some(action) = proposal.history_action else { return Ok(None) };
        if !proposal.edits.is_empty() {
            return Err("An undo or redo request must not contain direct edits.".into());
        }
        self.history_patch(actor, action).map(|(original, operations)| Some((original, operations.to_vec())))
    }

    pub(super) fn history_patch(&self, actor: ActorId, action: HistoryAction) -> Result<(u64, &[Operation]), String> {
        let (edit, inverse) = history_entry(self.history.iter().map(|(commit, _)| commit), &self.inverses, actor, action)?;
        for operation in inverse {
            let available = match operation {
                Operation::SetTextBatchActive { story, actor, original_sequence, active } => {
                    self.replica.can_activate_text_batch(*story, actor, *original_sequence, *active)
                }
                Operation::SetAutomergeTextBatchActive { story, actor, original_sequence, active } => {
                    self.replica.can_activate_automerge_text_batch(*story, actor, *original_sequence, *active)
                }
                _ => true,
            };
            if !available {
                return Err("The text story or edit is no longer available for undo or redo.".into());
            }
        }
        let overlaps = self.history.iter().skip(edit.effective as usize).any(|(commit, _)| {
            commit.actor != actor && commit.proposal.edits.iter().any(|later| inverse.iter().any(|op| history_conflicts(op, later)))
        });
        if overlaps {
            return Err("A collaborator changed the same target after your edit.".into());
        }
        Ok((edit.original, inverse))
    }
}

fn history_conflicts(inverse: &Operation, later: &Operation) -> bool {
    // The check above already established that the story survived repagination.
    !(inverse.is_text_compensation() && matches!(later, Operation::Page { .. })) && inverse.conflicts(later)
}

/// A guest or journal may apply a text toggle only as the recorded author's expected undo or redo.
pub(super) fn verify_text_compensation<'a>(
    commit: &Commit,
    history: impl Iterator<Item = &'a Commit>,
    inverses: &[Option<Vec<Operation>>],
) -> Result<(), String> {
    if !commit.proposal.edits.iter().any(Operation::is_text_compensation) {
        return Ok(());
    }
    let invalid = "A text undo or redo does not match the author's confirmed edit history.";
    let action = commit.proposal.history_action.ok_or(invalid)?;
    let (entry, expected) = history_entry(history, inverses, commit.actor, action).map_err(|_| invalid)?;
    if commit.correction != Some(entry.original)
        || commit.proposal.base.checked_add(1) != Some(commit.revision)
        || commit.proposal.edits.as_slice() != expected
    {
        return Err(invalid.into());
    }
    Ok(())
}

fn history_entry<'a, 'b>(
    history: impl Iterator<Item = &'a Commit>,
    inverses: &'b [Option<Vec<Operation>>],
    actor: ActorId,
    action: HistoryAction,
) -> Result<(OwnEdit, &'b [Operation]), &'static str> {
    let (own, last_new_edit) = own_edits(history, actor);
    let edit = match action {
        HistoryAction::Undo => own.into_iter().rev().find(|entry| !entry.undone),
        HistoryAction::Redo => own.into_iter().filter(|entry| entry.undone && entry.effective > last_new_edit).max_by_key(|entry| entry.effective),
    }
    .ok_or("There is no matching shared edit to undo or redo.")?;
    let inverse = inverses
        .get(edit.effective as usize - 1)
        .and_then(Option::as_deref)
        .ok_or("Undo and redo currently support simple text edits, frame properties, order and comment resolution.")?;
    Ok((edit, inverse))
}

fn own_edits<'a>(history: impl Iterator<Item = &'a Commit>, actor: ActorId) -> (Vec<OwnEdit>, u64) {
    let mut entries: Vec<OwnEdit> = Vec::new();
    let mut last_new_edit = 0;
    for commit in history.filter(|commit| commit.actor == actor) {
        match (commit.correction, commit.proposal.history_action) {
            (Some(target), Some(action)) => {
                if let Ok(index) = entries.binary_search_by_key(&target, |entry| entry.original) {
                    let entry = &mut entries[index];
                    entry.effective = commit.revision;
                    entry.undone = action == HistoryAction::Undo;
                }
            }
            (None, None) => {
                last_new_edit = commit.revision;
                entries.push(OwnEdit { original: commit.revision, effective: commit.revision, undone: false });
            }
            _ => {}
        }
    }
    (entries, last_new_edit)
}
