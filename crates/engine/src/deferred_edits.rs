//! App-thread boundary for an editor that requires approval before committing an edit.
use std::sync::Arc;

use designcraft_doc::{Document, LayerId, Selection};

use crate::{CommandSpec, EngineError, Result, Session};

pub struct DeferredEdit {
    pub before: Arc<Document>,
    pub after: Arc<Document>,
    pub before_selection: Selection,
    pub before_layer: LayerId,
    pub after_selection: Selection,
}

impl DeferredEdit {
    pub fn new_text_selection(&self) -> Option<designcraft_doc::TextSel> {
        self.after_selection.text.filter(|text| self.before.story(text.story).is_none() && text.frame.is_some() && text.cell.is_none())
    }
}

pub(crate) struct DeferredEdits {
    uid: u64,
    ready: bool,
    durable: bool,
    pending: Option<DeferredEdit>,
    placement: Option<Vec<u8>>,
}

impl Session {
    /// Enable deferred commits for one open document, or restore standalone operation.
    pub fn defer_document_edits(&mut self, uid: Option<u64>) {
        self.deferred_edits = uid.map(|uid| DeferredEdits { uid, ready: false, durable: false, pending: None, placement: None });
    }

    pub fn set_deferred_edit_durable(&mut self, durable: bool) {
        if let Some(edits) = self.deferred_edits.as_mut() {
            edits.durable = durable;
        }
    }

    pub(crate) fn has_encrypted_recovery(&self, uid: u64) -> bool {
        self.deferred_edits.as_ref().is_some_and(|edits| edits.uid == uid && edits.durable)
    }

    pub fn set_deferred_edit_ready(&mut self, ready: bool) {
        if let Some(edits) = self.deferred_edits.as_mut() {
            edits.ready = ready && edits.pending.is_none();
        }
    }

    /// Run the normal placement command with bytes already delivered by the file picker.
    /// Keeping them out of the command parameters avoids a second, base64-encoded copy.
    pub fn place_shared_bytes(&mut self, params: &serde_json::Value, bytes: Vec<u8>) -> Result<serde_json::Value> {
        if !self.is_deferring() {
            return Err(EngineError::Other("The shared document is no longer active.".into()));
        }
        if let Some(edits) = &mut self.deferred_edits {
            edits.placement = Some(bytes);
        }
        let result = self.execute("file.place", params);
        self.take_shared_placement();
        result
    }

    pub(crate) fn take_shared_placement(&mut self) -> Option<Vec<u8>> {
        self.deferred_edits.as_mut()?.placement.take()
    }

    pub fn take_deferred_edit(&mut self) -> Option<DeferredEdit> {
        self.deferred_edits.as_mut()?.pending.take()
    }

    pub fn deferred_edit_story(&self) -> Option<designcraft_doc::StoryId> {
        self.deferred_edits.as_ref()?.pending.as_ref()?.before_selection.text.map(|text| text.story)
    }

    pub fn has_deferred_text_frame(&self) -> bool {
        self.deferred_edits.as_ref().and_then(|edits| edits.pending.as_ref()).and_then(DeferredEdit::new_text_selection).is_some()
    }

    pub fn has_deferred_edit(&self) -> bool {
        self.deferred_edits.as_ref().is_some_and(|edits| edits.pending.is_some())
    }

    pub fn document_by_uid_mut(&mut self, uid: u64) -> Option<&mut crate::DocState> {
        self.docs.iter_mut().find(|st| st.uid == uid)
    }

    /// Whether the active document requires a confirmed room edit before changing.
    pub fn is_deferring(&self) -> bool {
        self.active().is_some_and(|st| self.is_deferring_uid(st.uid))
    }

    pub(crate) fn is_deferring_uid(&self, uid: u64) -> bool {
        self.deferred_edits.as_ref().is_some_and(|deferred| deferred.uid == uid)
    }

    pub(crate) fn check_deferred_command(&self, spec: &CommandSpec) -> Result<()> {
        if !self.is_deferring() {
            return Ok(());
        }
        let id = spec.id;
        if self.active().is_some_and(|st| st.interaction.is_some())
            && (matches!(id, "file.save" | "file.saveAs" | "file.saveACopy") || id.starts_with("file.export"))
        {
            return Err(EngineError::Other("Finish the current gesture before saving or exporting the shared document.".into()));
        }
        if is_deferred_edit_command(id) {
            return if self.deferred_edits.as_ref().is_some_and(|edits| edits.ready) {
                Ok(())
            } else {
                Err(EngineError::Other("Wait for the shared document to catch up before editing.".into()))
            };
        }
        if matches!(id, "edit.undo" | "edit.redo" | "file.revert" | "data.preview")
            || (spec.undoable || spec.journal)
                && !(id.starts_with("selection.")
                    || id.starts_with("select.")
                    || id.starts_with("file.export")
                    || id.starts_with("text.select")
                    || matches!(id, "text.move" | "text.extendTo" | "text.exitToFrame" | "edit.copy" | "edit.selectAll" | "edit.deselectAll")
                    || matches!(
                        id,
                        "tool.select"
                            | "layer.activate"
                            | "layout.selectPage"
                            | "file.activate"
                            | "file.new"
                            | "file.open"
                            | "file.close"
                            | "file.save"
                            | "file.saveAs"
                            | "file.saveACopy"
                            | "file.recovery.save"
                            | "font.reload"
                    ))
        {
            return Err(EngineError::Other("This command is unavailable while editing a shared document.".into()));
        }
        Ok(())
    }

    /// Restore the confirmed state and retain a proposal for the application to validate.
    pub(crate) fn defer_current_edit(
        &mut self,
        before: Arc<Document>,
        selection: Selection,
        revision: u64,
        active_layer: LayerId,
        propose: bool,
    ) -> Result<()> {
        let Some(st) = self.active_mut() else { return Ok(()) };
        st.active_layer = active_layer;
        if Arc::ptr_eq(&st.doc, &before) {
            return Ok(());
        }
        let after = std::mem::replace(&mut st.doc, before.clone());
        let after_selection = std::mem::replace(&mut st.selection, selection.clone());
        st.revision = revision;
        if !propose {
            return Ok(());
        }
        let Some(edits) = self.deferred_edits.as_mut().filter(|edits| edits.ready && edits.pending.is_none()) else {
            return Err(EngineError::Other("Wait for the shared document to catch up before editing.".into()));
        };
        edits.ready = false;
        edits.pending = Some(DeferredEdit { before, after, before_selection: selection, before_layer: active_layer, after_selection });
        Ok(())
    }
}

pub(crate) fn is_deferred_edit_command(id: &str) -> bool {
    matches!(
        id,
        "file.place"
            | "frame.create"
            | "line.create"
            | "edit.clear"
            | "text.placeCaret"
            | "text.insert"
            | "text.delete"
            | "text.deleteSurrounding"
            | "type.char"
            | "type.para"
            | "type.paragraphLanguage"
            | "type.align"
            | "type.alignLeft"
            | "type.alignCenter"
            | "type.alignRight"
            | "type.justify"
            | "type.justifyAll"
            | "layout.pages.insert"
            | "layout.pages.delete"
            | "layer.new"
            | "layer.set"
            | "layer.move"
            | "layer.delete"
            | "layer.deleteUnused"
            | "layer.merge"
            | "layer.others"
            | "layer.duplicate"
            | "transform.move"
            | "transform.resize"
            | "transform.rotate"
            | "transform.scale"
            | "transform.set"
            | "transform.flip"
            | "transform.shear"
            | "transform.clear"
            | "object.fill"
            | "object.stroke"
            | "object.arrange"
            | "object.setLayer"
            | "object.reorder"
            | "object.opacity"
            | "object.setFlags"
            | "style.object.apply"
            | "style.paragraph.apply"
            | "object.bringToFront"
            | "object.bringForward"
            | "object.sendBackward"
            | "object.sendToBack"
    )
}
