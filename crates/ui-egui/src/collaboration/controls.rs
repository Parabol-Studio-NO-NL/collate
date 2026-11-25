use std::ops::Range;

use designcraft_collab::{CommentAnchor, Operation, SharedId};
use designcraft_doc::{Document, StoryId, TextSel};
use designcraft_engine::Session;
use egui::{Button, Context, TextEdit, Ui, Window};
use serde_json::json;

use super::{Collaboration, Mode, Room, history::HistoryAction, presence};
use crate::{DesignApp, i18n::tr};

enum Action {
    Connect(bool),
    StartNewRoom,
    OpenSavedRoomCopy,
    ReceiveSeed,
    CancelDownload,
    Reconnect,
    Disconnect,
    History(HistoryAction),
    Comment(Box<Operation>),
    SelectCommentText(StoryId, Range<usize>),
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
                    self.collaboration.message = if host && self.collaboration.room_conflict.is_some() { None } else { Some(error) };
                }
            }
            Some(Action::StartNewRoom) => {
                if let Err(error) = self.start_new_room_from_current() {
                    self.collaboration.message = Some(error);
                }
            }
            Some(Action::OpenSavedRoomCopy) => {
                if let Err(error) = self.open_saved_room_copy() {
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
                    && let Err(error) = room.clear_pending(&mut self.session)
                {
                    room.stop_with_error(&error);
                    return;
                }
                self.collaboration.room = None;
                self.collaboration.comment.clear();
                self.collaboration.reply_to = None;
                self.collaboration.comment_needs_text_selection = false;
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
                let posted = matches!(&*edit, Operation::CreateComment { .. } | Operation::Reply { .. });
                match room.queue_local(&mut self.session, vec![*edit]) {
                    Ok(()) if posted => {
                        self.collaboration.comment.clear();
                        self.collaboration.reply_to = None;
                        self.collaboration.comment_needs_text_selection = false;
                    }
                    Ok(()) => {}
                    Err(error) => room.error = Some(error),
                }
            }
            Some(Action::SelectCommentText(story, range)) => {
                if self.run("tool.select", json!({"tool": "type"})).is_ok()
                    && self.run("text.select", json!({"story": story.0, "anchor": range.start, "focus": range.end})).is_ok()
                    && let Some(st) = self.session.active()
                {
                    let layout = self.session.cache.get(&st.doc, story, None);
                    let location = designcraft_compose::caret(&layout, range.start)
                        .and_then(|(frame, ..)| layout.frames.get(frame))
                        .and_then(|frame| st.doc.find(frame.frame));
                    if let Some(loc) = location
                        && let Some(item) = st.doc.item_at(&loc)
                    {
                        let bounds = st.doc.parent_xf(&loc).transform_rect_bbox(item.bounds());
                        crate::canvas::go_to_spread_rect(self, loc.spread, bounds);
                    }
                }
            }
            None => {}
        }
    }

    pub(crate) fn collaboration_button(&mut self, ui: &mut Ui) {
        let label = if self.collaboration.room.is_some() { "Sharing…" } else { "Share" };
        let button = Button::new(crate::rtl::widget(ui, tr(&self.ui.language, label))).min_size(egui::vec2(0.0, 24.0));
        if ui.add(button).clicked() {
            let _ = self.run("app.shareDocument", serde_json::Value::Null);
        }
    }
}

fn show(c: &mut Collaboration, session: &Session, lang: &str, ui: &mut Ui, current_page: usize, history: [bool; 2]) -> Option<Action> {
    if let Some(joining) = &c.joining {
        if let Some(progress) = joining.artwork_progress() {
            artwork_progress(ui, lang, progress);
        } else {
            ui.label(tr(lang, "Receiving the shared document…"));
        }
        return ui.button(tr(lang, "Cancel")).clicked().then_some(Action::CancelDownload);
    }
    let Some(room) = c.room.as_mut() else {
        ui.label(tr(lang, "Open a saved document to host a room, or use an invitation to join."));
        ui.label(tr(lang, "Your name"));
        ui.add(TextEdit::singleline(&mut c.name).char_limit(60).hint_text(tr(lang, "Your name")));
        ui.checkbox(&mut c.relay, tr(lang, "Allow internet relay"));
        let has_name = !c.name.trim().is_empty();
        let has_document = session.active().is_some();
        let host = if let Some(conflict) = c.room_conflict.as_mut().filter(|conflict| conflict.applies(session)) {
            let message = ui.label(tr(
                lang,
                "This document differs from the saved room. Starting a new room keeps the previous room and creates a new invitation.",
            ));
            ui.ctx().accesskit_node_builder(message.id, |node| node.set_live(egui::accesskit::Live::Polite));
            let start = ui.add_enabled(has_name, room_button(lang, "Start a new room from this version"));
            if std::mem::take(&mut conflict.focus_start) && start.enabled() {
                start.request_focus();
            }
            if start.clicked() {
                Some(Action::StartNewRoom)
            } else if ui.add(room_button(lang, "Open saved room copy")).clicked() {
                Some(Action::OpenSavedRoomCopy)
            } else {
                None
            }
        } else {
            let host = ui.add_enabled(has_name && has_document, Button::new(tr(lang, "Start sharing"))).clicked().then_some(Action::Connect(true));
            if cfg!(target_os = "macos") && session.recovery_dir.is_some() && session.active().is_some_and(|state| state.path.is_some()) {
                let recovery = ui.collapsing(tr(lang, "Room recovery"), |ui| ui.add(room_button(lang, "Open saved room copy")).clicked());
                if recovery.body_returned == Some(true) { Some(Action::OpenSavedRoomCopy) } else { host }
            } else {
                host
            }
        };
        ui.separator();
        ui.label(tr(lang, "Invitation code"));
        ui.add(TextEdit::singleline(&mut c.join_code).password(true).char_limit(4096).hint_text(tr(lang, "Invitation code")));
        let can_join = has_name && !c.join_code.trim().is_empty();
        let download = ui.add_enabled(can_join, Button::new(tr(lang, "Join and receive document"))).clicked();
        let join = ui.add_enabled(can_join && has_document, Button::new(tr(lang, "Join with open document"))).clicked();
        if let Some(message) = &c.message {
            ui.label(tr(lang, message));
        }
        return host.or(if download { Some(Action::ReceiveSeed) } else { join.then_some(Action::Connect(false)) });
    };

    if let Some(progress) = room.artwork_progress().filter(|_| room.online && !room.fatal) {
        artwork_progress(ui, lang, progress);
    } else {
        let message = ui.label(tr(lang, room_status(room, session)));
        ui.ctx().accesskit_node_builder(message.id, |node| node.set_live(egui::accesskit::Live::Polite));
    }
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
    let pending = !room.pending.is_empty() || room.typing.is_some() || session.has_deferred_edit();
    if (room.paused_typing().is_some() || room.story_queue_blocked) && ui.button(tr(lang, "Copy queued typing")).clicked() {
        let mut typing = room.declined_story_text();
        typing.push_str(room.paused_typing().unwrap_or_default());
        if !typing.is_empty() {
            ui.ctx().copy_text(typing);
        }
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

fn room_status(room: &Room, session: &Session) -> &'static str {
    if room.fatal {
        return "Connection stopped";
    }
    if room.story_queue_blocked {
        return "Some edits need attention";
    }
    if room.story_draft.is_some() {
        return match (room.online, room.storage.is_durable()) {
            (true, _) => "Syncing your edits…",
            (false, true) => "Offline. Edits are saved on this Mac.",
            (false, false) => "Offline. Your edits are waiting to sync.",
        };
    }
    if !room.online && room.can_draft_story() {
        return "Offline. Edits are saved on this Mac.";
    }
    if room.paused_typing().is_some() {
        return "Queued typing paused";
    }
    if !room.pending.is_empty() || room.typing.is_some() || session.has_deferred_edit() {
        return "Waiting for the host to confirm your edit";
    }
    match (room.can_edit(), room.online, &room.mode) {
        (true, true, _) => "Connected",
        (true, false, _) => "Offline. You can queue one edit.",
        (false, _, Mode::Host { .. }) if room.local.is_some() => "Room history is full",
        _ => "Connecting…",
    }
}

fn room_button<'a>(lang: &str, label: &'a str) -> Button<'a> {
    Button::new(tr(lang, label)).min_size(egui::vec2(0.0, 24.0))
}

fn artwork_progress(ui: &mut Ui, lang: &str, progress: super::attachments::ArtworkProgress) {
    let template = if progress.sending { "Sending artwork ({percent}%)" } else { "Receiving artwork ({percent}%)" };
    let fraction = progress.bytes as f64 / progress.total.max(1) as f64;
    let label = tr(lang, template).replace("{percent}", &((fraction * 100.0) as u32).to_string());
    ui.add(egui::ProgressBar::new(fraction as f32).text(label));
}

fn comments(c: &mut Collaboration, st: &designcraft_engine::DocState, lang: &str, ui: &mut Ui, current_page: usize) -> Option<Action> {
    let room = c.room.as_ref()?;
    let enabled = room.can_edit();
    let display_replica = room.story_draft.as_ref().map_or(&room.replica, |draft| &draft.visible);
    let mut action = None;
    egui::ScrollArea::vertical().id_salt("shared-comments").max_height(220.0).show(ui, |ui| {
        for thread in room.replica.comments() {
            ui.push_id(&thread.id, |ui| {
                ui.small(author(room, &thread.id));
                ui.label(&thread.text);
                let target = display_replica.comment_text_range(&st.doc, &thread.anchor);
                let orphaned = if thread.anchor.is_text() { target.is_none() } else { display_replica.comment_is_orphaned(&st.doc, thread) };
                if orphaned {
                    ui.small(tr(lang, "Anchor removed"));
                }
                for reply in &thread.replies {
                    ui.indent(&reply.id, |ui| {
                        ui.small(author(room, &reply.id));
                        ui.label(&reply.text);
                    });
                }
                ui.horizontal_wrapped(|ui| {
                    if let Some((story, range)) = target
                        && ui.button(tr(lang, "Select text")).clicked()
                    {
                        action = Some(Action::SelectCommentText(story, range));
                    }
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
    let text_selection = st.selection.text.filter(|selection| !selection.is_caret());
    let anchor = if let Some(selection) = text_selection {
        selected_text_anchor(ui, room, st, selection)
    } else if c.comment_needs_text_selection {
        None
    } else {
        st.selection
            .items
            .first()
            .and_then(|id| room.replica.shared_id(*id))
            .map(CommentAnchor::Object)
            .or_else(|| page_anchor(st.selection.page.unwrap_or(current_page), &st.doc))
    };
    if c.reply_to.is_none() {
        if text_selection.is_some() {
            let caption = if anchor.is_some() { "Attached to selected text" } else { "This text selection can’t be anchored." };
            ui.small(tr(lang, caption));
        } else if c.comment_needs_text_selection {
            ui.small(tr(lang, "Select text to reattach this comment."));
        }
    }
    ui.add_enabled(
        enabled,
        TextEdit::multiline(&mut c.comment)
            .desired_rows(2)
            .char_limit(1000)
            .hint_text(tr(lang, if c.reply_to.is_some() { "Write a reply" } else { "Write a comment" })),
    );
    if c.comment.is_empty() {
        c.comment_needs_text_selection = false;
    }
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct CommentSelection {
    document: u64,
    document_revision: u64,
    room_revision: u64,
    actor: designcraft_collab_core::ActorId,
    selection: TextSel,
}

fn selected_text_anchor(ui: &Ui, room: &Room, st: &designcraft_engine::DocState, selection: TextSel) -> Option<CommentAnchor> {
    if selection.cell.is_some() {
        return None;
    }
    let key = ui.make_persistent_id("text-comment-selection");
    let source = CommentSelection {
        document: st.uid,
        document_revision: st.revision,
        room_revision: room.revision,
        actor: room.local.as_ref()?.actor,
        selection,
    };
    if let Some((saved, anchor)) = ui.data(|data| data.get_temp::<(CommentSelection, Option<CommentAnchor>)>(key))
        && saved == source
    {
        return anchor;
    }
    let anchor = room.replica.text_comment_anchor(&st.doc, selection.story, selection.range());
    ui.data_mut(|data| data.insert_temp(key, (source, anchor.clone())));
    anchor
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
