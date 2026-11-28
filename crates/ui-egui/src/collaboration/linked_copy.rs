use std::sync::{Arc, mpsc};
use std::time::Duration;

use designcraft_collab::RoomProtocol;
use designcraft_doc::{Document, Storage};
use designcraft_engine::{DocState, FontScope, Session, file_access};
use serde_json::{Value, json};

use super::oversized::{ReducedImage, reduce_images};
use crate::{DesignApp, app::file_dialogs::Bound, i18n::tr};

pub(super) type CopyResult = mpsc::Receiver<Result<SharedCopy, String>>;

pub(super) struct SharedCopy {
    document: Arc<Document>,
    path: String,
    fonts: Option<Arc<FontScope>>,
    reduced: Vec<ReducedImage>,
}

pub(super) fn has_linked_artwork(document: &Document) -> bool {
    document.assets.values().any(|asset| asset.storage == Storage::Linked)
}

impl DesignApp {
    pub(super) fn ask_share_linked_copy(&mut self) -> Result<(), String> {
        let source = self.session.active().ok_or("Open a document before sharing a copy.")?;
        let name = tr(&self.ui.language, "{name} Shared Copy.designcraft").replace("{name}", &source.doc.title);
        self.ask_save(&name, Bound::Document, |app, path| {
            app.start_linked_copy(path)?;
            Ok(Value::Null)
        })?;
        Ok(())
    }

    pub(super) fn start_linked_copy(&mut self, destination: String) -> Result<(), String> {
        if self.collaboration.linked_copy.is_some() || self.collaboration.room.is_some() || self.collaboration.joining.is_some() {
            return Err("Finish the current sharing action first.".into());
        }
        let source = self.session.active().ok_or("Open a document before sharing a copy.")?;
        if source.interaction.is_some() || source.preview_stash.is_some() {
            return Err("Finish the current document edit before sharing a copy.".into());
        }
        let document = source.doc.clone();
        let path = source.path.clone();
        let roots = file_access::current();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("collate-linked-copy".into())
            .spawn(move || {
                let result = file_access::confine(roots.as_ref(), || prepare_copy(&document, path, destination));
                let _ = sender.send(result);
            })
            .map_err(|_| "Could not start preparing the shared copy.")?;
        self.collaboration.linked_copy = Some(receiver);
        self.collaboration.message = None;
        Ok(())
    }

    pub(super) fn poll_linked_copy(&mut self, ctx: &egui::Context) {
        let Some(work) = &self.collaboration.linked_copy else { return };
        let result = match work.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(40));
                return;
            }
            Err(mpsc::TryRecvError::Disconnected) => Err("Could not prepare the shared copy.".into()),
        };
        self.collaboration.linked_copy = None;
        if let Err(error) = result.and_then(|copy| self.open_linked_copy(copy)) {
            self.collaboration.message = Some(error);
        }
    }

    fn open_linked_copy(&mut self, copy: SharedCopy) -> Result<(), String> {
        self.session.prefs.note_recent_file(&copy.path);
        let mut state = DocState::new((*copy.document).clone(), Some(copy.path));
        state.fonts = copy.fonts;
        self.session.add_document(state);
        self.after_engine();
        self.connect_collaboration(true)?;
        if let Some(room) = self.collaboration.room.as_mut() {
            room.notice = super::oversized::reduced_message(&copy.reduced);
        }
        Ok(())
    }
}

fn prepare_copy(document: &Document, path: Option<String>, destination: String) -> Result<SharedCopy, String> {
    super::seed_document::validate_document_structure(document, RoomProtocol::Automerge)?;
    let mut session = Session::new();
    session.add_document(DocState::new(document.clone(), path));
    let saved = session.execute("file.saveACopy", &json!({"path": destination, "verifiedLinks": true})).map_err(|error| error.to_string())?;
    let path = saved["path"].as_str().ok_or("Could not find the saved shared copy.")?.to_string();
    session.execute("file.open", &json!({"path": path})).map_err(|error| error.to_string())?;
    // Linked originals are embedded now, so every oversized image has bytes to reduce.
    let opened = session.doc_mut().map_err(|error| error.to_string())?;
    let mut document = (*opened.doc).clone();
    let reduced = reduce_images(&mut document, |_| true)?;
    if !reduced.is_empty() {
        opened.doc = Arc::new(document);
        opened.bump_revision();
        session.execute("file.save", &json!({"path": path})).map_err(|error| error.to_string())?;
    }
    let opened = session.doc().map_err(|error| error.to_string())?;
    Ok(SharedCopy { document: opened.doc.clone(), path, fonts: opened.fonts.clone(), reduced })
}
