use designcraft_doc::{Document, StoryId, build::NewDocument};
use designcraft_geom::Rect;
use serde_json::json;

use super::{document, editor, join, room, wait_for};
use crate::DesignApp;

const SEED_TEXT: &str = "ABCD";

fn text_seed() -> (Document, StoryId) {
    let mut seed = Document::new(&NewDocument::default());
    let layer = seed.default_layer();
    let (_, story) =
        seed.add_text_frame(designcraft_doc::SpreadRef::Doc(0), Rect::new(36.0, 36.0, 300.0, 250.0), layer, SEED_TEXT, Default::default()).unwrap();
    (seed, story)
}

fn open_room(host: &mut DesignApp, guest: &mut DesignApp) {
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut *host], "host starts sharing", |apps| room(apps[0]).can_edit() && room(apps[0]).invitation.is_some());
    join(guest, room(host).invitation.as_deref().unwrap());
    wait_for(&mut [host, guest], "guest can edit", |apps| room(apps[1]).can_edit());
}

fn select(app: &mut DesignApp, story: StoryId, anchor: usize, focus: usize) {
    app.run("text.select", json!({"story": story.0, "anchor": anchor, "focus": focus})).unwrap();
}

fn insert(app: &mut DesignApp, text: &str) {
    app.run("text.insert", json!({"text": text, "raw": true})).unwrap();
}

fn text(app: &DesignApp, story: StoryId) -> &str {
    &document(app).story(story).unwrap().text
}

#[test]
fn guest_edits_twice_without_waiting_and_keeps_the_caret_through_overlap() {
    let (seed, story) = text_seed();
    let mut host = editor(&seed, "Host");
    let mut guest = editor(&seed, "Guest");
    open_room(&mut host, &mut guest);
    let ctx = egui::Context::default();

    select(&mut guest, story, 2, 2);
    insert(&mut guest, "Ω");
    assert_eq!(text(&guest, story), "ABΩCD");
    guest.poll_collaboration(&ctx);
    assert_eq!(room(&guest).pending.len(), 1);

    // This edit uses the caret from the previous draft and depends on its Automerge change.
    insert(&mut guest, "β");
    assert_eq!(text(&guest, story), "ABΩβCD");
    guest.poll_collaboration(&ctx);
    assert_eq!(room(&guest).pending.len(), 2);
    assert_eq!(text(&host, story), SEED_TEXT);
    assert_eq!(room(&host).revision, 0);

    let frame = json!({"rect": [350, 36, 500, 250], "content": "unassigned"});
    assert!(guest.run("frame.create", frame.clone()).is_err());
    assert_eq!(document(&guest).spreads[0].items.len(), seed.spreads[0].items.len());
    assert_eq!(text(&guest, story), "ABΩβCD");

    // The host has never polled the proposals and replaces the original B and C.
    // The latter's deletion also exercises restoring the guest caret at a changed boundary.
    select(&mut host, story, 1, 3);
    insert(&mut host, "λ");
    wait_for(&mut [&mut host], "the host receives both pipelined drafts before the guest reads acknowledgements", |apps| {
        guest.collaboration.room.as_mut().unwrap().advance_proposal();
        room(apps[0]).revision == 3
    });
    assert_eq!(room(&guest).revision, 0);
    assert_eq!(room(&guest).pending.len(), 2);
    wait_for(&mut [&mut host, &mut guest], "both queued changes and the overlapping replacement converge", |apps| {
        apps.iter().all(|app| room(app).revision == 3 && room(app).pending.is_empty())
    });
    let merged = text(&guest, story);
    assert_eq!(merged, text(&host, story));
    assert!(merged.starts_with('A') && merged.ends_with('D'));
    assert!(merged.contains('λ') && merged.contains("Ωβ"));
    assert!(!merged.contains(['B', 'C']));
    let caret = guest.session.doc().unwrap().selection.text.unwrap();
    assert!(caret.is_caret());
    assert_eq!(caret.story, story);
    assert!(merged.get(..caret.focus).is_some_and(|before| before.ends_with("Ωβ")), "caret moved away from the guest's last insertion");

    guest.run("frame.create", frame).unwrap();
    wait_for(&mut [&mut host, &mut guest], "structure can change again after the drafts are confirmed", |apps| {
        apps.iter().all(|app| room(app).revision == 4 && room(app).pending.is_empty())
    });
    assert_eq!(document(&host), document(&guest));
    assert_eq!(document(&host).spreads[0].items.len(), seed.spreads[0].items.len() + 1);
}

#[test]
fn an_ephemeral_guest_keeps_its_unconfirmed_text_but_cannot_edit_offline() {
    let (seed, story) = text_seed();
    let mut host = editor(&seed, "Host");
    let mut guest = editor(&seed, "Guest without recovery storage");
    open_room(&mut host, &mut guest);
    assert!(!room(&guest).storage.is_durable());

    select(&mut guest, story, SEED_TEXT.len(), SEED_TEXT.len());
    insert(&mut guest, "Ω");
    guest.poll_collaboration(&egui::Context::default());
    assert_eq!(room(&guest).pending.len(), 1);
    assert_eq!(room(&host).revision, 0);
    drop(host);

    wait_for(&mut [&mut guest], "guest detects that the host shut down", |apps| !room(apps[0]).online);
    assert_eq!(text(&guest, story), "ABCDΩ");
    assert_eq!(room(&guest).pending.len(), 1);
    assert!(guest.run("text.insert", json!({"text": "β", "raw": true})).is_err());
    assert_eq!(text(&guest, story), "ABCDΩ");
    assert!(guest.collaboration_blocks_close());
}

#[test]
fn deleting_the_story_elsewhere_pauses_the_queue_and_keeps_a_recoverable_draft() {
    let (seed, story) = text_seed();
    let frame = seed.story(story).unwrap().frames[0];
    let mut host = editor(&seed, "Host");
    let mut guest = editor(&seed, "Guest");
    open_room(&mut host, &mut guest);

    let ctx = egui::Context::default();
    select(&mut guest, story, SEED_TEXT.len(), SEED_TEXT.len());
    insert(&mut guest, "Ω");
    guest.poll_collaboration(&ctx);
    insert(&mut guest, "β");
    guest.poll_collaboration(&ctx);
    assert_eq!(room(&guest).pending.len(), 2);

    // The last frame and its story disappear before the host accepts either local draft.
    host.run("edit.clear", json!({"ids": [frame.0]})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "the host removes the story and the guest pauses its drafts", |apps| {
        apps.iter().all(|app| room(app).revision == 1) && room(apps[1]).story_queue_blocked
    });
    assert!(document(&host).story(story).is_none());
    assert!(room(&guest).story_draft.as_ref().unwrap().confirmed.story(story).is_none());
    assert_eq!(text(&guest, story), "ABCDΩβ");
    assert_eq!(room(&guest).pending.len(), 2);
    assert!(guest.run("text.insert", json!({"text": " discarded", "raw": true})).is_err());
    assert!(guest.collaboration_blocks_close());

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("Recovered story.designcraft");
    guest.run("file.saveACopy", json!({"path": &path})).unwrap();
    let mut recovered = editor(&seed, "Standalone recovery copy");
    recovered.run("file.open", json!({"path": &path})).unwrap();
    assert_eq!(text(&recovered, story), "ABCDΩβ");
}

// Called by the native restart fixture while its disposable test Keychain is active.
#[cfg(target_os = "macos")]
pub(super) fn durable_guest_replays_offline_queue(root: &std::path::Path) {
    use designcraft_engine::Session;

    use crate::Services;

    let directory = root.join("Offline text queue");
    std::fs::create_dir(&directory).unwrap();
    let (seed, story) = text_seed();
    let mut initial = editor(&seed, "Offline story fixture");
    let path = |label: &str| directory.join(format!("{label}.designcraft"));
    initial.run("file.saveAs", json!({"path": path("Host")})).unwrap();
    std::fs::copy(path("Host"), path("Guest")).unwrap();
    drop(initial);

    let reopen = |label: &str| {
        let mut session = Session::new();
        session.recovery_dir = Some(directory.join(label).join("Recovery"));
        let mut app = DesignApp::new(session, Services::default());
        app.collaboration.name = label.into();
        app.run("file.open", json!({"path": path(label)})).unwrap();
        app
    };
    let mut host = reopen("Host");
    let mut guest = reopen("Guest");
    open_room(&mut host, &mut guest);
    assert!(room(&guest).storage.is_durable());
    let guest_actor = room(&guest).local.as_ref().unwrap().actor;
    drop(host);
    wait_for(&mut [&mut guest], "durable guest detects shutdown and can still edit", |apps| !room(apps[0]).online && room(apps[0]).can_edit());

    let ctx = egui::Context::default();
    select(&mut guest, story, SEED_TEXT.len(), SEED_TEXT.len());
    insert(&mut guest, "Ω");
    guest.poll_collaboration(&ctx);
    insert(&mut guest, "β");
    guest.poll_collaboration(&ctx);
    assert_eq!(room(&guest).pending.len(), 2);
    assert_eq!(text(&guest, story), "ABCDΩβ");
    assert!(guest.run("file.save", json!({})).is_err());
    let recovery_copy = path("Offline copy");
    assert!(guest.run("file.saveAs", json!({"path": &recovery_copy})).is_err());
    assert!(!recovery_copy.exists());
    guest.run("file.saveACopy", json!({"path": &recovery_copy})).unwrap();
    let mut standalone = DesignApp::new(Session::new(), Services::default());
    standalone.run("file.open", json!({"path": &recovery_copy})).unwrap();
    assert_eq!(text(&standalone, story), "ABCDΩβ");
    assert!(!guest.collaboration_blocks_close());
    guest.run("file.close", json!({})).unwrap();
    drop(guest);

    let mut host = reopen("Host");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "reopened host starts sharing", |apps| room(apps[0]).can_edit() && room(apps[0]).invitation.is_some());
    select(&mut host, story, 0, 0);
    insert(&mut host, "λ");
    wait_for(&mut [&mut host], "host edits before the guest reopens", |apps| room(apps[0]).revision == 1);

    let mut guest = reopen("Guest");
    assert_eq!(text(&guest, story), SEED_TEXT);
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    assert_eq!(room(&guest).pending.len(), 2);
    wait_for(&mut [&mut host, &mut guest], "recovered offline changes are delivered once", |apps| {
        apps.iter().all(|app| room(app).revision == 3 && room(app).pending.is_empty())
    });
    assert_eq!(room(&guest).local.as_ref().unwrap().actor, guest_actor);
    assert_eq!(text(&host, story), "λABCDΩβ");
    assert_eq!(document(&host), document(&guest));
}
