use designcraft_collab::{CommentAnchor, Operation, SharedId};
use designcraft_doc::Document;
use designcraft_engine::Session;
use egui::{Button, Context, TextEdit, Ui, Window};

use super::{Collaboration, Mode, Room, history::HistoryAction, presence};
use crate::{DesignApp, i18n::tr};

enum Action {
    Connect(bool),
    ReceiveSeed,
    CancelDownload,
    Reconnect,
    Disconnect,
    History(HistoryAction),
    Comment(Box<Operation>),
}

impl DesignApp {
    pub(crate) fn show_collaboration(&mut self, ctx: &Context) {
        let language = &self.ui.language;
        let current_page = crate::canvas::current_page(self).unwrap_or(0);
        let history = [self.shared_history_enabled("edit.undo"), self.shared_history_enabled("edit.redo")].map(|flag| flag.unwrap_or(false));
        let c = &mut self.collaboration;
        let mut open = c.open;
        if !open {
            return;
        }
        let mut action = None;
        Window::new(tr(language, "Share Document"))
            .id(egui::Id::unique("share-document"))
            .open(&mut open)
            .collapsible(false)
            .default_width(320.0)
            .show(ctx, |ui| action = show(c, &self.session, language, ui, current_page, history));
        c.open = open;
        match action {
            Some(Action::Connect(host)) => {
                if let Err(error) = self.connect_collaboration(host) {
                    self.collaboration.message = Some(error);
                }
            }
            Some(Action::ReceiveSeed) => {
                if let Err(error) = self.start_seed_download() {
                    self.collaboration.message = Some(error);
                }
            }
            Some(Action::CancelDownload) => {
                self.collaboration.joining = None;
            }
            Some(Action::Reconnect) => {
                if let Err(error) = self.reconnect_collaboration()
                    && let Some(room) = self.collaboration.room.as_mut()
                {
                    room.error = Some(error);
                }
            }
            Some(Action::Disconnect) => {
                if let Some(room) = self.collaboration.room.as_mut()
                    && let Err(error) = room.clear_pending()
                {
                    room.stop_with_error(&error);
                    return;
                }
                self.collaboration.room = None;
                self.collaboration.comment.clear();
                self.collaboration.reply_to = None;
                self.collaboration.confirm_discard = false;
                self.session.defer_document_edits(None);
            }
            Some(Action::History(action)) => {
                let command = if action == HistoryAction::Undo { "edit.undo" } else { "edit.redo" };
                if let Some(Err(error)) = self.shared_history_command(command)
                    && let Some(room) = self.collaboration.room.as_mut()
                {
                    room.error = Some(error);
                }
            }
            Some(Action::Comment(edit)) => {
                let Some(room) = self.collaboration.room.as_mut() else { return };
                match room.queue_local(&mut self.session, vec![*edit]) {
                    Ok(()) => {
                        self.collaboration.comment.clear();
                        self.collaboration.reply_to = None;
                    }
                    Err(error) => room.error = Some(error),
                }
            }
            None => {}
        }
    }

    pub(crate) fn collaboration_button(&mut self, ui: &mut Ui, rect: egui::Rect) {
        let label = if self.collaboration.room.is_some() { "Sharing…" } else { "Share…" };
        if ui.put(rect, Button::new(tr(&self.ui.language, label))).clicked() {
            self.open_collaboration_dialog();
        }
    }
}

fn show(c: &mut Collaboration, session: &Session, lang: &str, ui: &mut Ui, current_page: usize, history: [bool; 2]) -> Option<Action> {
    if c.joining.is_some() {
        ui.label(tr(lang, "Receiving the shared document…"));
        return ui.button(tr(lang, "Cancel")).clicked().then_some(Action::CancelDownload);
    }
    let Some(room) = c.room.as_mut() else {
        ui.label(tr(lang, "Open a saved document to host a room, or use an invitation to join."));
        ui.label(tr(lang, "Your name"));
        ui.add(TextEdit::singleline(&mut c.name).char_limit(60).hint_text(tr(lang, "Your name")));
        ui.checkbox(&mut c.relay, tr(lang, "Allow internet relay"));
        let has_name = !c.name.trim().is_empty();
        let has_document = session.active().is_some();
        let host = ui.add_enabled(has_name && has_document, Button::new(tr(lang, "Start sharing"))).clicked();
        ui.separator();
        ui.label(tr(lang, "Invitation code"));
        ui.add(TextEdit::singleline(&mut c.join_code).password(true).char_limit(4096).hint_text(tr(lang, "Invitation code")));
        let can_join = has_name && !c.join_code.trim().is_empty();
        let download = ui.add_enabled(can_join, Button::new(tr(lang, "Join and receive document"))).clicked();
        let join = ui.add_enabled(can_join && has_document, Button::new(tr(lang, "Join with open document"))).clicked();
        if let Some(message) = &c.message {
            ui.label(tr(lang, message));
        }
        return if download { Some(Action::ReceiveSeed) } else { (host || join).then_some(Action::Connect(host)) };
    };

    let status = if room.fatal {
        "Connection stopped"
    } else if room.paused_typing().is_some() {
        "Queued typing paused"
    } else if room.pending.is_some() || room.typing.is_some() || session.has_deferred_edit() {
        "Waiting for the host to confirm your edit"
    } else if room.can_edit() && room.online {
        "Connected"
    } else if room.can_edit() {
        "Offline. You can queue one edit."
    } else {
        match room.mode {
            Mode::Host { .. } if room.local.is_some() => "Room history is full",
            _ => "Connecting…",
        }
    };
    ui.label(tr(lang, status));
    if let Some(error) = &room.error {
        ui.label(tr(lang, error));
    }
    if let Some(code) = &room.invitation
        && ui.button(tr(lang, "Copy invitation code")).clicked()
    {
        ui.ctx().copy_text(code.clone());
    }
    for peer in room.peers.values() {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 1.0, presence::color(peer.actor));
            ui.label(&peer.name);
        });
    }

    let mut action = None;
    if matches!(room.mode, Mode::Guest { .. }) && (!room.online || room.fatal) {
        ui.label(tr(lang, "New invitation code"));
        ui.add(TextEdit::singleline(&mut c.join_code).password(true).char_limit(4096).hint_text(tr(lang, "New invitation code")));
        if ui.add_enabled(!c.join_code.trim().is_empty(), Button::new(tr(lang, "Reconnect"))).clicked() {
            action = Some(Action::Reconnect);
        }
    }
    if let Some(st) = session.active().filter(|st| st.uid == room.doc_uid) {
        #[cfg(target_os = "macos")]
        if st.path.is_none() {
            ui.label(tr(lang, "Save a local copy to enable room recovery."));
        }
        ui.horizontal(|ui| {
            if ui.add_enabled(history[0], Button::new(tr(lang, "Undo my edit"))).clicked() {
                action = Some(Action::History(HistoryAction::Undo));
            }
            if ui.add_enabled(history[1], Button::new(tr(lang, "Redo my edit"))).clicked() {
                action = Some(Action::History(HistoryAction::Redo));
            }
        });
        ui.separator();
        ui.strong(tr(lang, "Comments"));
        action = comments(c, st, lang, ui, current_page).or(action);
    }
    let Some(room) = c.room.as_ref() else { return action };
    let pending = room.pending.is_some() || room.typing.is_some() || session.has_deferred_edit();
    if let Some(typing) = room.paused_typing()
        && ui.button(tr(lang, "Copy queued typing")).clicked()
    {
        ui.ctx().copy_text(typing.into());
    }
    ui.separator();
    if !pending && ui.button(tr(lang, "Leave session")).clicked() {
        return Some(Action::Disconnect);
    }
    if pending && ui.button(tr(lang, "Discard unconfirmed edit…")).clicked() {
        c.confirm_discard = true;
    }
    if pending && c.confirm_discard {
        ui.label(tr(lang, "Leaving may discard an edit the host has not confirmed."));
        ui.horizontal(|ui| {
            if ui.button(tr(lang, "Keep waiting")).clicked() {
                c.confirm_discard = false;
            }
            if ui.button(tr(lang, "Discard and leave")).clicked() {
                action = Some(Action::Disconnect);
            }
        });
    }
    action
}

fn comments(c: &mut Collaboration, st: &designcraft_engine::DocState, lang: &str, ui: &mut Ui, current_page: usize) -> Option<Action> {
    let room = c.room.as_ref()?;
    let enabled = room.can_edit();
    let mut action = None;
    egui::ScrollArea::vertical().id_salt("shared-comments").max_height(220.0).show(ui, |ui| {
        for thread in room.replica.comments() {
            ui.push_id(&thread.id, |ui| {
                ui.small(author(room, &thread.id));
                ui.label(&thread.text);
                if room.replica.comment_is_orphaned(&st.doc, thread) {
                    ui.small(tr(lang, "Anchor removed"));
                }
                for reply in &thread.replies {
                    ui.indent(&reply.id, |ui| {
                        ui.small(author(room, &reply.id));
                        ui.label(&reply.text);
                    });
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(enabled, Button::new(tr(lang, "Reply"))).clicked() {
                        c.reply_to = Some(thread.id.clone());
                    }
                    let resolve_label = if thread.resolved { "Reopen" } else { "Resolve" };
                    if ui.add_enabled(enabled, Button::new(tr(lang, resolve_label))).clicked() {
                        action = Some(Action::Comment(Box::new(Operation::SetResolved { thread: thread.id.clone(), resolved: !thread.resolved })));
                    }
                });
                ui.separator();
            });
        }
    });
    let local = room.local.as_ref()?;
    let anchor = st
        .selection
        .items
        .first()
        .and_then(|id| room.replica.shared_id(*id))
        .map(CommentAnchor::Object)
        .or_else(|| page_anchor(st.selection.page.unwrap_or(current_page), &st.doc));
    ui.add_enabled(
        enabled,
        TextEdit::multiline(&mut c.comment)
            .desired_rows(2)
            .char_limit(1000)
            .hint_text(tr(lang, if c.reply_to.is_some() { "Write a reply" } else { "Write a comment" })),
    );
    let can_post = enabled && (c.reply_to.is_some() || anchor.is_some()) && !c.comment.trim().is_empty();
    ui.horizontal(|ui| {
        if ui.add_enabled(can_post, Button::new(tr(lang, "Post"))).clicked() {
            let id = SharedId::Created { actor: local.actor.to_string(), sequence: room.next_sequence, index: 0 };
            let text = c.comment.trim().to_owned();
            let edit = match c.reply_to.as_ref() {
                Some(thread) => Some(Operation::Reply { thread: thread.clone(), id, text }),
                None => anchor.map(|anchor| Operation::CreateComment { id, anchor, text }),
            };
            action = edit.map(Box::new).map(Action::Comment);
        }
        if c.reply_to.is_some() && ui.button(tr(lang, "Cancel reply")).clicked() {
            c.reply_to = None;
        }
    });
    action
}

fn page_anchor(index: usize, doc: &Document) -> Option<CommentAnchor> {
    doc.spreads.iter().flat_map(|spread| &spread.pages).nth(index).map(|page| CommentAnchor::Page(page.id))
}

fn author(room: &Room, id: &SharedId) -> String {
    let Some(actor) = id.author() else { return String::new() };
    room.peers
        .values()
        .chain(room.local.iter())
        .find(|peer| peer.actor.to_string() == actor)
        .map_or_else(|| actor.chars().take(12).collect(), |peer| peer.name.clone())
}
