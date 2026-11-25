//! Authenticated Automerge transactions in the experimental room protocol.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use designcraft_collab::{Error, Operation, Replica, automerge_stories::StoryTransaction, text::TextSplice};
use designcraft_collab_core::ActorId;
use designcraft_doc::{Document, StoryId, TextSel};
use designcraft_engine::{DeferredEdit, DocState};
use web_time::Instant;

use super::{
    Commit, Message, Proposal,
    edit_wire::{SendCursor, WireEdit},
    encode_edit, operation_story,
};

pub(super) struct PendingSelection {
    pub(super) sequence: u64,
    selection: TextSel,
    anchor: Vec<u8>,
    focus: Vec<u8>,
}

pub(super) struct Captured {
    pub(super) transaction: StoryTransaction,
    pub(super) selection: Option<PendingSelection>,
    pub(super) replica: Replica,
}

/// The editor displays the queued branch. Sync, replay, and room fingerprints use `confirmed`.
pub(super) struct Draft {
    pub(super) confirmed: Arc<Document>,
    pub(super) visible: Replica,
}

#[derive(Default)]
pub(super) struct Sender {
    active: Option<(u64, WireEdit, SendCursor)>,
    submitted: Option<u64>,
    awaiting_since: Option<Instant>,
}

impl Sender {
    pub(super) fn acknowledge(&mut self, pending: &VecDeque<Proposal>) {
        if pending.is_empty() {
            *self = Self::default();
        } else {
            self.awaiting_since = Some(Instant::now());
        }
    }

    pub(super) fn advance(&mut self, pending: &VecDeque<Proposal>, mut send: impl FnMut(Vec<u8>) -> Result<(), String>) -> Result<(), String> {
        let Some(front) = pending.front() else { return Ok(()) };
        if self.active.as_ref().is_some_and(|(sequence, _, _)| *sequence < front.sequence) {
            // A delayed acknowledgement arrived during a redundant resend. The receiver
            // replaces any partial transfer before accepting a new fragmented proposal.
            self.active = None;
        }
        if self.active.is_none() && self.awaiting_since.is_some_and(|time| time.elapsed() >= Duration::from_secs(2)) {
            self.submitted = None;
            self.awaiting_since = None;
        }
        for _ in 0..32 {
            if self.active.is_none() {
                let next = pending.iter().find(|proposal| self.submitted.is_none_or(|sequence| proposal.sequence > sequence));
                let Some(proposal) = next else { break };
                // The next change on the same story depends on the previous one. Wait across
                // stories so a rejected head cannot leave a later independent edit accepted.
                if self.submitted.is_some_and(|sequence| sequence >= front.sequence)
                    && (only_story(front).is_none() || only_story(front) != only_story(proposal))
                {
                    break;
                }
                self.active = Some((proposal.sequence, encode_edit(Message::Propose(proposal.clone()))?, SendCursor::default()));
            }
            let Some((sequence, wire, cursor)) = &mut self.active else { break };
            match wire.send_next(cursor, &mut send) {
                Ok(true) => {
                    self.submitted = Some(*sequence);
                    self.awaiting_since.get_or_insert_with(Instant::now);
                    self.active = None;
                }
                Ok(false) => {}
                Err(_) => break,
            }
        }
        Ok(())
    }
}

fn only_story(proposal: &Proposal) -> Option<StoryId> {
    let [change] = proposal.story_transaction.as_ref()?.changes.as_slice() else { return None };
    Some(change.story)
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
    Ok(Some(Captured { transaction, selection, replica: draft }))
}

pub(super) fn rebase(
    confirmed: &Arc<Document>,
    replica: &Replica,
    actor: ActorId,
    pending: &VecDeque<Proposal>,
) -> Result<(Arc<Document>, Option<Draft>), String> {
    if !pending.front().is_some_and(|p| p.story_transaction.is_some()) {
        return Ok((confirmed.clone(), None));
    }
    let mut document = (**confirmed).clone();
    let mut visible = replica.clone();
    for proposal in pending {
        let transaction = proposal.story_transaction.as_ref().ok_or("A story outbox also contains structural changes.")?;
        visible.apply_automerge_text(&mut document, &actor.0, transaction).map_err(|error| error.to_string())?;
    }
    Ok((Arc::new(document), Some(Draft { confirmed: confirmed.clone(), visible })))
}

pub(super) fn remap_selection(state: &DocState, old: &Replica, updated: &Replica) -> Option<TextSel> {
    let selection = state.selection.text?;
    let [anchor, focus] = old.automerge_anchors(&state.doc, selection.story, [selection.anchor, selection.focus])?;
    resolve(updated, selection, &anchor, &focus)
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
