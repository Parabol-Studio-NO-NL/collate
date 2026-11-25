use std::ops::Range;

use designcraft_collab::{CommentAnchor, Operation, SharedId};
use designcraft_doc::{Document, StoryId, build::NewDocument};
use designcraft_engine::Session;
use serde_json::json;

use super::{document, editor, join, room, wait_for, wait_for_revision};
use crate::{DesignApp, Services, collaboration::replay};

fn room_with_text(text: &str) -> (DesignApp, DesignApp, Document, StoryId, String) {
    let mut host = editor(&Document::new(&NewDocument::default()), "Editor");
    let frame = host.run("frame.create", json!({"rect": [40, 50, 300, 200], "content": "text", "text": text})).unwrap();
    let story = StoryId(frame["story"].as_u64().unwrap());
    let seed = document(&host).clone();
    let mut guest = editor(&seed, "Proofreader");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "editor opens the room", |apps| room(apps[0]).can_edit() && room(apps[0]).invitation.is_some());
    let invitation = room(&host).invitation.clone().unwrap();
    join(&mut guest, &invitation);
    wait_for(&mut [&mut host, &mut guest], "proofreader joins", |apps| room(apps[1]).can_edit());
    (host, guest, seed, story, invitation)
}

fn select(app: &mut DesignApp, story: StoryId, range: Range<usize>) {
    app.run("text.select", json!({"story": story.0, "anchor": range.start, "focus": range.end})).unwrap();
}

fn post_comment(app: &mut DesignApp, story: StoryId, range: Range<usize>) -> (SharedId, CommentAnchor) {
    let anchor = room(app).replica.text_comment_anchor(document(app), story, range).unwrap();
    assert!(matches!(&anchor, CommentAnchor::ReversibleText { story: target, .. } if *target == story));
    let room = app.collaboration.room.as_mut().unwrap();
    let thread = SharedId::Created { actor: room.local.as_ref().unwrap().actor.to_string(), sequence: room.next_sequence, index: 0 };
    room.queue_local(
        &mut app.session,
        vec![Operation::CreateComment { id: thread.clone(), anchor: anchor.clone(), text: "Verify this original letter".into() }],
    )
    .unwrap();
    (thread, anchor)
}

#[test]
fn overlapping_deletes_keep_original_text_hidden_until_both_authors_undo() {
    let (mut host, mut guest, seed, story, invitation) = room_with_text("AéBβZ");
    let (thread, anchor) = post_comment(&mut host, story, 3..4);
    wait_for_revision(&mut host, &mut guest, "comment on the original B", 1);

    let verify = |app: &DesignApp, expected_text: &str, visible_range: Option<Range<usize>>| {
        let doc = document(app);
        let replica = &room(app).replica;
        let comment = replica.comments().find(|comment| comment.id == thread).unwrap();
        assert_eq!(doc.story(story).unwrap().text, expected_text);
        assert_eq!(comment.anchor, anchor);
        assert_eq!(replica.comment_is_orphaned(doc, comment), visible_range.is_none());
        assert_eq!(replica.comment_text_range(doc, &comment.anchor), visible_range.map(|range| (story, range)));
    };

    // Both deletions start from the same revision and include the same original B.
    select(&mut host, story, 1..4);
    host.run("text.delete", json!({})).unwrap();
    select(&mut guest, story, 3..6);
    guest.run("text.delete", json!({})).unwrap();
    wait_for_revision(&mut host, &mut guest, "both independent deletes are visible", 3);
    for app in [&host, &guest] {
        verify(app, "AZ", None);
    }

    host.run("edit.undo", json!({})).unwrap();
    wait_for_revision(&mut host, &mut guest, "undo the editor's deletion and keep the proofreader's deletion", 4);
    for app in [&host, &guest] {
        verify(app, "AéZ", None);
    }

    guest.run("edit.undo", json!({})).unwrap();
    wait_for_revision(&mut host, &mut guest, "both undos revive the original comment target", 5);
    for app in [&host, &guest] {
        verify(app, "AéBβZ", Some(3..4));
    }

    let commits: Vec<_> = room(&host).history.iter().map(|(commit, _)| commit).collect();
    let encoded = serde_json::to_vec(&commits).unwrap();
    let recovered = replay::restore(&seed, &seed, serde_json::from_slice(&encoded).unwrap()).unwrap();
    assert_eq!(&recovered.document, document(&host));
    let comment = recovered.replica.comments().find(|comment| comment.id == thread).unwrap();
    assert_eq!(comment.anchor, anchor);
    assert_eq!(recovered.replica.comment_text_range(&recovered.document, &comment.anchor), Some((story, 3..4)));

    let mut late = DesignApp::new(Session::new(), Services::default());
    late.collaboration.name = "Late proofreader".into();
    late.collaboration.join_code = invitation;
    late.start_seed_download().unwrap();
    wait_for(&mut [&mut host, &mut guest, &mut late], "late proofreader receives the revived original target", |apps| {
        apps[2].collaboration.room.as_ref().is_some_and(|room| room.can_edit() && room.revision == 5)
    });
    verify(&late, "AéBβZ", Some(3..4));

    host.run("edit.redo", json!({})).unwrap();
    wait_for(&mut [&mut host, &mut guest, &mut late], "redo is delivered to the late proofreader", |apps| {
        apps.iter().all(|app| room(app).revision == 6)
    });
    for app in [&host, &guest, &late] {
        verify(app, "AβZ", None);
    }
    guest.run("edit.redo", json!({})).unwrap();
    wait_for(&mut [&mut host, &mut guest, &mut late], "the second redo keeps shared originals hidden", |apps| {
        apps.iter().all(|app| room(app).revision == 7)
    });
    for app in [&host, &guest, &late] {
        verify(app, "AZ", None);
    }
}

#[test]
fn redoing_an_older_format_write_keeps_its_rank_below_the_later_write() {
    let (mut host, mut guest, _, story, _) = room_with_text("AB");
    let older = 19.0;
    let later = 27.0;
    select(&mut host, story, 0..1);
    host.run("type.char", json!({"attrs": {"size": older}})).unwrap();
    wait_for_revision(&mut host, &mut guest, "editor changes the size first", 1);

    select(&mut guest, story, 0..1);
    guest.run("type.char", json!({"attrs": {"underline": true}})).unwrap();
    wait_for_revision(&mut host, &mut guest, "proofreader underlines the same letter", 2);
    select(&mut guest, story, 0..1);
    guest.run("type.char", json!({"attrs": {"size": later}})).unwrap();
    wait_for_revision(&mut host, &mut guest, "proofreader also changes the size", 3);

    let verify = |host: &DesignApp, guest: &DesignApp, expected_size| {
        assert_eq!(document(host), document(guest));
        let current = document(host).story(story).unwrap();
        assert_eq!(current.text, "AB");
        let format = current.char_format_of(0);
        assert_eq!(format.over.size, Some(expected_size));
        assert_eq!(format.over.underline, Some(true));
    };
    verify(&host, &guest, later);
    host.run("edit.undo", json!({})).unwrap();
    wait_for_revision(&mut host, &mut guest, "undo the older size", 4);
    verify(&host, &guest, later);
    host.run("edit.redo", json!({})).unwrap();
    wait_for_revision(&mut host, &mut guest, "redo cannot move the older size above the later size", 5);
    verify(&host, &guest, later);

    guest.run("edit.undo", json!({})).unwrap();
    wait_for_revision(&mut host, &mut guest, "undo the later size and preserve the independent underline", 6);
    verify(&host, &guest, older);
    guest.run("edit.redo", json!({})).unwrap();
    wait_for_revision(&mut host, &mut guest, "redo restores the later size and preserves underline", 7);
    verify(&host, &guest, later);
}
