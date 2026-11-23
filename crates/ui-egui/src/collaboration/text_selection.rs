use designcraft_collab::{
    Operation, Replica,
    text::{AtomId, TextEdit},
};
use designcraft_doc::TextSel;
use designcraft_engine::DocState;

pub(super) fn after(state: &DocState, previous: &Replica, updated: &Replica, operations: &[Operation], own_edit: bool) -> Option<TextSel> {
    let mut selection = state.selection.text?;
    let operation = operations.iter().find(|op| super::operation_story(op) == Some(selection.story))?;
    let mut collapsed = false;
    if own_edit && let Operation::Text { actor, sequence, edits, .. } = operation {
        let inserted: usize = edits.iter().map(TextEdit::inserted_atom_count).sum();
        if let Some(last) = inserted.checked_sub(1).and_then(|index| u16::try_from(index).ok()) {
            let last = AtomId::Inserted { actor: actor.as_str().into(), sequence: *sequence, index: last };
            let position = updated.text_byte_after(selection.story, Some(&last))?;
            selection.anchor = position;
            selection.focus = position;
            return Some(selection);
        }
        collapsed = edits.iter().any(|edit| matches!(edit, TextEdit::Delete { .. }));
    }
    let anchor = previous.text_anchor(&state.doc, selection.story, selection.anchor)?;
    let focus = previous.text_anchor(&state.doc, selection.story, selection.focus)?;
    selection.anchor = updated.text_byte_after(selection.story, anchor.as_ref())?;
    selection.focus = updated.text_byte_after(selection.story, focus.as_ref())?;
    if collapsed {
        let position = selection.anchor.min(selection.focus);
        selection.anchor = position;
        selection.focus = position;
    }
    Some(selection)
}
