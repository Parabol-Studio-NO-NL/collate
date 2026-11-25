use designcraft_collab::{Operation, Replica, SharedId, text::TextEdit};
use designcraft_doc::{Document, Selection, TextSel};
use designcraft_engine::{DeferredEdit, Session};
use serde_json::json;

use super::{Proposal, Room};
use crate::DesignApp;

const MAX_BUFFERED_BYTES: usize = 512;

pub(super) struct BufferedTyping {
    target: Option<TypingTarget>,
    text: String,
}

#[derive(Clone, Copy, PartialEq)]
enum TypingTarget {
    Caret(TextSel),
    NewFrame,
}

pub(super) struct FrameEntry {
    pub(super) sequence: u64,
    frame: SharedId,
    origin: Selection,
    anchor: usize,
    focus: usize,
}

impl FrameEntry {
    pub(super) fn capture(sequence: u64, edit: &DeferredEdit, operations: &[Operation]) -> Option<Self> {
        let caret = edit.new_text_selection()?;
        // Commands that enter a new text frame create or convert a single frame.
        let frame = operations.iter().find_map(Operation::new_text_frame)?.clone();
        Some(Self { sequence, frame, origin: edit.before_selection.clone(), anchor: caret.anchor, focus: caret.focus })
    }

    fn confirmed(&self, replica: &Replica, doc: &Document, selection: &Selection) -> Option<TextSel> {
        if self.origin != *selection {
            return None;
        }
        let frame = replica.local_id(&self.frame)?;
        let story = doc.item(frame)?.text_frame()?.story;
        let text = &doc.story(story)?.text;
        if !text.is_char_boundary(self.anchor) || !text.is_char_boundary(self.focus) {
            return None;
        }
        Some(TextSel { story, anchor: self.anchor, focus: self.focus, frame: Some(frame), cell: None })
    }
}

impl DesignApp {
    fn shared_text_frame_pending(&self) -> bool {
        self.collaboration.room.as_ref().is_some_and(|room| {
            self.session.active().is_some_and(|st| st.uid == room.doc_uid) && (room.frame_entry.is_some() || self.session.has_deferred_text_frame())
        })
    }

    pub(crate) fn buffer_pending_frame_input(&mut self, event: &egui::Event, composing: bool) -> bool {
        if !self.shared_text_frame_pending() {
            return false;
        }
        match event {
            egui::Event::Ime(egui::ImeEvent::Commit(text)) => self.buffer_shared_typing(text),
            egui::Event::Text(text) | egui::Event::Paste(text) if !composing => self.buffer_shared_typing(text),
            _ => false,
        }
    }

    /// Preserve input that arrives while a previous canvas text edit is being confirmed.
    pub(crate) fn buffer_shared_typing(&mut self, text: &str) -> bool {
        let new_frame = self.shared_text_frame_pending();
        let Some(room) = self.collaboration.room.as_mut() else { return false };
        let Some(st) = self.session.active().filter(|st| st.uid == room.doc_uid) else { return false };
        let target = if new_frame {
            TypingTarget::NewFrame
        } else {
            let Some(selection) = st.selection.text else { return false };
            let pending_text = room.pending.as_ref().is_some_and(|proposal| proposal.affected_stories().any(|id| id == selection.story));
            if !pending_text && self.session.deferred_edit_story() != Some(selection.story) && room.typing.is_none() {
                return false;
            }
            TypingTarget::Caret(selection)
        };
        let input = room.typing.get_or_insert_with(|| BufferedTyping { target: Some(target), text: String::new() });
        if input.target == Some(target) && input.text.len().saturating_add(text.len()) <= MAX_BUFFERED_BYTES {
            input.text.push_str(text);
        } else {
            self.status("Typing is waiting for confirmation. Review the shared document before entering more text.");
        }
        true
    }
}

impl Room {
    pub(super) fn flush_typing(&mut self, session: &mut Session) {
        if !self.can_edit() || session.has_deferred_edit() {
            return;
        }
        let Some(input) = self.typing.as_ref().filter(|input| input.target.is_some()) else { return };
        let Some(st) = session.active().filter(|st| st.uid == self.doc_uid) else { return };
        if st.selection.text.map(TypingTarget::Caret) != input.target {
            self.pause_typing();
            return;
        }
        match session.execute("text.insert", &json!({ "text": input.text })) {
            Ok(_) => self.typing = None,
            Err(error) => {
                self.pause_typing();
                self.error = Some(error.to_string());
            }
        }
    }

    pub(super) fn update_typing_selection(&mut self, before: Option<TextSel>, updated: Option<TextSel>) {
        if before != updated
            && let Some(input) = self.typing.as_mut()
            && let Some(TypingTarget::Caret(target)) = input.target
        {
            if before == Some(target)
                && let Some(caret) = updated
            {
                input.target = Some(TypingTarget::Caret(caret));
            } else {
                self.pause_typing();
            }
        }
    }

    pub(super) fn confirm_frame_entry(&mut self, session: &mut Session) {
        let Some(entry) = self.frame_entry.take() else { return };
        let caret = session.document_by_uid_mut(self.doc_uid).and_then(|st| {
            let caret = entry.confirmed(&self.replica, &st.doc, &st.selection)?;
            st.selection = Selection::text(caret);
            Some(caret)
        });
        if let Some(input) = self.typing.as_mut().filter(|input| input.target == Some(TypingTarget::NewFrame)) {
            if let Some(caret) = caret {
                input.target = Some(TypingTarget::Caret(caret));
            } else {
                self.pause_typing();
            }
        }
    }

    pub(super) fn pause_typing(&mut self) {
        if let Some(input) = &mut self.typing {
            input.target = None;
            self.error = Some("Queued typing is paused. Copy it to keep your text or leave the room to discard it.".into());
        }
    }

    pub(super) fn preserve_rejected_typing(&mut self, submitted: String) {
        if !submitted.is_empty() {
            let input = self.typing.get_or_insert_with(|| BufferedTyping { target: None, text: String::new() });
            input.text.insert_str(0, &submitted);
        }
        self.pause_typing();
    }

    pub(super) fn paused_typing(&self) -> Option<&str> {
        self.typing.as_ref().filter(|input| input.target.is_none()).map(|input| input.text.as_str())
    }
}

pub(super) fn submitted_typing(proposal: &Proposal) -> String {
    if let Some(transaction) = &proposal.story_transaction {
        return transaction.inserted_text().unwrap_or_default();
    }
    proposal
        .edits
        .iter()
        .filter_map(|op| match op {
            Operation::Text { edits, .. } => Some(edits),
            _ => None,
        })
        .flatten()
        .filter_map(|edit| match edit {
            TextEdit::Insert { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
