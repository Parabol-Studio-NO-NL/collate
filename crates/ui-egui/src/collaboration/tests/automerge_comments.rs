use std::ops::Range;

use designcraft_collab::{CommentAnchor, Operation, SharedId};
use designcraft_doc::{Document, StoryId, build::NewDocument};
use designcraft_engine::Session;
use serde_json::json;

use super::{document, editor, join, room, wait_for, wait_for_revision};
use crate::{DesignApp, Services, collaboration::replay};

#[test]
fn a_seed_text_comment_tracks_original_glyphs_through_wire_replay() {
    let mut host = editor(&Document::new(&NewDocument::default()), "Book editor");
    let frame = host.run("frame.create", json!({"rect": [40, 50, 300, 200], "content": "text", "text": "AéβZ"})).unwrap();
    let story = StoryId(frame["story"].as_u64().unwrap());
    let seed = document(&host).clone();
    let mut guest = editor(&seed, "Proofreader");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "editor opens the room", |apps| room(apps[0]).can_edit() && room(apps[0]).invitation.is_some());
    let invitation = room(&host).invitation.clone().unwrap();
    join(&mut guest, &invitation);
    wait_for(&mut [&mut host, &mut guest], "proofreader joins the room", |apps| room(apps[1]).can_edit());

    // The seed has not yet been edited or installed as Automerge state in this room.
    let anchor = room(&guest).replica.text_comment_anchor(document(&guest), story, 1..5).unwrap();
    assert!(matches!(&anchor, CommentAnchor::ReversibleText { story: target, .. } if *target == story));
    let reviewer = guest.collaboration.room.as_mut().unwrap();
    let thread = SharedId::Created { actor: reviewer.local.as_ref().unwrap().actor.to_string(), sequence: reviewer.next_sequence, index: 0 };
    reviewer
        .queue_local(
            &mut guest.session,
            vec![Operation::CreateComment { id: thread.clone(), anchor: anchor.clone(), text: "Check the accents".into() }],
        )
        .unwrap();
    wait_for_revision(&mut host, &mut guest, "both peers receive the comment on seed text", 1);

    let verify = |app: &DesignApp, expected: &str, range: Option<Range<usize>>| {
        let doc = document(app);
        let replica = &room(app).replica;
        let comment = replica.comments().find(|comment| comment.id == thread).unwrap();
        assert_eq!(comment.anchor, anchor);
        assert_eq!(comment.text, "Check the accents");
        assert_eq!(doc.story(story).unwrap().text, expected);
        assert_eq!(replica.comment_is_orphaned(doc, comment), range.is_none());
        assert_eq!(replica.comment_text_range(doc, &comment.anchor), range.map(|range| (story, range)));
    };
    for app in [&host, &guest] {
        verify(app, "AéβZ", Some(1..5));
    }

    // A later insertion lies between the two originals but cannot become part of the anchor.
    host.run("text.select", json!({"story": story.0, "anchor": 3})).unwrap();
    host.run("text.insert", json!({"text": "λ", "raw": true})).unwrap();
    wait_for_revision(&mut host, &mut guest, "insert inside the original range", 2);
    for app in [&host, &guest] {
        verify(app, "AéλβZ", Some(1..7));
    }

    host.run("text.select", json!({"story": story.0, "anchor": 1, "focus": 3})).unwrap();
    host.run("text.delete", json!({})).unwrap();
    wait_for_revision(&mut host, &mut guest, "the range shrinks to its surviving original", 3);
    for app in [&host, &guest] {
        verify(app, "AλβZ", Some(3..5));
    }

    // The replica must distinguish the newly inserted β even though the visible string is identical.
    host.run("text.select", json!({"story": story.0, "anchor": 3, "focus": 5})).unwrap();
    host.run("text.insert", json!({"text": "β", "raw": true})).unwrap();
    wait_for_revision(&mut host, &mut guest, "the identical replacement orphans the comment", 4);
    for app in [&host, &guest] {
        verify(app, "AλβZ", None);
    }

    let mut late = DesignApp::new(Session::new(), Services::default());
    late.collaboration.name = "Late proofreader".into();
    late.collaboration.join_code = invitation;
    late.start_seed_download().unwrap();
    wait_for(&mut [&mut host, &mut guest, &mut late], "late peer reconstructs the orphaned comment from its invitation", |apps| {
        apps[2].collaboration.room.as_ref().is_some_and(|room| room.can_edit() && room.revision == 4)
    });
    verify(&late, "AλβZ", None);

    let commits: Vec<_> = room(&host).history.iter().map(|(commit, _)| commit).collect();
    let encoded = serde_json::to_vec(&commits).unwrap();
    let recovered = replay::restore(&seed, &seed, serde_json::from_slice(&encoded).unwrap()).unwrap();
    assert_eq!(&recovered.document, document(&host));
    let comment = recovered.replica.comments().find(|comment| comment.id == thread).unwrap();
    assert_eq!(comment.anchor, anchor);
    assert!(recovered.replica.comment_is_orphaned(&recovered.document, comment));
}
