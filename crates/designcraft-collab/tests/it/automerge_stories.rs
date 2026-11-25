use designcraft_collab::{
    Error, Replica,
    automerge_stories::{AutomergeStories, StoryTransaction},
};
use designcraft_doc::{Hyperlink, HyperlinkDest, HyperlinkSource, StoryId};

const ALICE: [u8; 32] = [0xa1; 32];
const BOB: [u8; 32] = [0xb1; 32];

#[test]
fn independent_collators_merge_book_stories_and_replay_into_a_fresh_native_document() {
    let mut seed = super::seed();
    let (_, opening) = super::add_text_frame(&mut seed, 0.0);
    let (_, closing) = super::add_text_frame(&mut seed, 40.0);
    let (_, untouched) = super::add_text_frame(&mut seed, 80.0);
    seed.story_mut(opening).unwrap().insert(0, "Épreuve\n");

    let mut alice = AutomergeStories::default();
    let mut bob = AutomergeStories::default();
    let mut alice_book = seed.clone();
    let mut bob_book = seed.clone();
    alice_book.story_mut(opening).unwrap().insert("Épreuve".len(), " d’Alice");
    alice_book.story_mut(closing).unwrap().insert(0, "Colophon\n");
    let mut format = bob_book.story(opening).unwrap().chars[0].format.clone();
    format.over.underline = Some(true);
    bob_book.story_mut(opening).unwrap().insert_with("Épreuve".len(), " de Bob", format);

    let alice_tx = alice.capture(&seed, &alice_book, &ALICE, None).unwrap().unwrap();
    let alice_tx: StoryTransaction = serde_json::from_slice(&serde_json::to_vec(&alice_tx).unwrap()).unwrap();
    let bob_tx = bob.capture(&seed, &bob_book, &BOB, None).unwrap().unwrap();
    assert_eq!(bob_tx.inserted_text().unwrap(), " de Bob");
    alice.apply(&mut alice_book, &BOB, &bob_tx).unwrap();
    bob.apply(&mut bob_book, &ALICE, &alice_tx).unwrap();

    let mut reopened = seed.clone();
    let mut recovered = AutomergeStories::default();
    recovered.apply(&mut reopened, &BOB, &bob_tx).unwrap();
    recovered.apply(&mut reopened, &ALICE, &alice_tx).unwrap();
    for id in [opening, closing, untouched] {
        let a = alice_book.story(id).unwrap();
        for other in [&bob_book, &reopened] {
            let b = other.story(id).unwrap();
            assert_eq!((&a.text, &a.chars, &a.paras, &a.frames), (&b.text, &b.chars, &b.paras, &b.frames));
        }
    }
    let opening = reopened.story(opening).unwrap();
    assert!(opening.text.contains("d’Alice") && opening.text.contains("de Bob"));
    assert!(opening.chars.iter().any(|run| run.format.over.underline == Some(true)));
    assert!(reopened.story(closing).unwrap().text.starts_with("Colophon\n"));
    assert!(std::sync::Arc::ptr_eq(&reopened.stories[&untouched], &seed.stories[&untouched]));
    assert_eq!(reopened.spreads, seed.spreads);
}

#[test]
fn a_failed_multistory_merge_leaves_native_text_and_crdt_state_intact() {
    let mut seed = super::seed();
    let (_, first) = super::add_text_frame(&mut seed, 0.0);
    let (_, second) = super::add_text_frame(&mut seed, 40.0);
    let mut draft = seed.clone();
    draft.story_mut(first).unwrap().insert(0, "New heading ");
    draft.story_mut(second).unwrap().insert(0, "New colophon ");
    let mut sender = AutomergeStories::default();
    let transaction = sender.capture(&seed, &draft, &ALICE, None).unwrap().unwrap();
    let mut recipient = AutomergeStories::default();
    let mut native = seed.clone();

    let mut broken = transaction.clone();
    broken.changes[1].story = StoryId(second.0 + 50);
    assert_eq!(recipient.apply(&mut native, &ALICE, &broken), Err(Error::UnknownResource));
    assert!(recipient.apply(&mut native, &BOB, &transaction).is_err());
    assert_eq!(native, seed);
    recipient.apply(&mut native, &ALICE, &transaction).unwrap();
    assert_eq!(native.story(first).unwrap().text, draft.story(first).unwrap().text);
    assert_eq!(native.story(second).unwrap().text, draft.story(second).unwrap().text);
}

#[test]
fn a_peer_cannot_shift_native_hyperlink_offsets_without_reanchoring_them() {
    let mut seed = super::seed();
    let (_, story) = super::add_text_frame(&mut seed, 0.0);
    let mut draft = seed.clone();
    draft.story_mut(story).unwrap().insert(0, "New paragraph\n");
    let mut sender = AutomergeStories::default();
    let transaction = sender.capture(&seed, &draft, &ALICE, None).unwrap().unwrap();
    let id = seed.alloc();
    seed.hyperlinks.push(Hyperlink {
        id,
        name: "Further reading".into(),
        source: HyperlinkSource::Text { story, start: 0, end: 4 },
        dest: HyperlinkDest::Url("https://example.com".into()),
    });
    let original = seed.clone();
    let mut recipient = AutomergeStories::default();
    assert_eq!(recipient.apply(&mut seed, &ALICE, &transaction), Err(Error::UnsupportedEdit));
    assert_eq!(seed, original);
}

#[test]
fn native_room_replica_keeps_the_two_story_histories_separate() {
    let mut seed = super::seed();
    let (_, migrated) = super::add_text_frame(&mut seed, 0.0);
    let (_, legacy) = super::add_text_frame(&mut seed, 40.0);
    let mut writer = Replica::new(&seed);
    let mut draft = seed.clone();
    draft.story_mut(migrated).unwrap().insert(0, "Chapter\n");
    let first = writer.capture_automerge_text(&seed, &draft, &ALICE, None).unwrap().unwrap();
    let cursor = writer.automerge_cursor(migrated, "Chapter\n".len()).unwrap();

    let mut recipient = Replica::new(&seed);
    let mut book = seed.clone();
    let mut next = book.clone();
    next.story_mut(legacy).unwrap().insert(0, "Legacy note\n");
    let old_operations = recipient.capture(&book, &next, "legacy", 1).unwrap();
    recipient.apply(&mut book, &old_operations).unwrap();
    recipient.apply_automerge_text(&mut book, &ALICE, &first).unwrap();
    assert_eq!(recipient.resolve_automerge_cursor(migrated, &cursor), Some("Chapter\n".len()));
    assert!(book.story(legacy).unwrap().text.starts_with("Legacy note\n"));

    let mut next = book.clone();
    next.story_mut(migrated).unwrap().insert(0, "Legacy attempt\n");
    assert_eq!(recipient.capture(&book, &next, "legacy", 2), Err(Error::UnsupportedEdit));

    let mut wrong_replica = Replica::new(&seed);
    let mut attempt = seed.clone();
    attempt.story_mut(legacy).unwrap().insert(0, "Concurrent attempt\n");
    let crossed = wrong_replica.capture_automerge_text(&seed, &attempt, &BOB, None).unwrap().unwrap();
    assert_eq!(recipient.apply_automerge_text(&mut book, &BOB, &crossed), Err(Error::UnsupportedEdit));
    assert!(book.story(legacy).unwrap().text.starts_with("Legacy note\n"));
}

#[cfg(not(feature = "automerge-rooms"))]
#[test]
fn protocol_15_rejects_automerge_comment_anchors_without_changing_the_story_model() {
    use designcraft_collab::{CommentAnchor, Operation, automerge_text::AutomergeText};

    let mut book = super::seed();
    let (_, story) = super::add_text_frame(&mut book, 0.0);
    let mut recipient = Replica::new(&book);
    let native = book.story(story).unwrap();
    let anchor = AutomergeText::seed(native, &book.styles, &ALICE).unwrap().comment_anchor(0..4).unwrap();
    let incoming = Operation::CreateComment {
        id: super::id("alice", 1),
        anchor: CommentAnchor::AutomergeText { story, anchor },
        text: "Review the opening".into(),
    };

    let incoming = serde_json::from_slice(&serde_json::to_vec(&incoming).unwrap()).unwrap();
    assert_eq!(recipient.apply(&mut book, &[incoming]), Err(Error::UnsupportedEdit));
    assert_eq!(recipient.comments().count(), 0);
    assert!(matches!(recipient.text_comment_anchor(&book, story, 0..4), Some(CommentAnchor::Text { .. })));
}

#[cfg(feature = "automerge-rooms")]
#[test]
fn an_automerge_comment_from_a_pristine_story_survives_replay_and_retains_its_thread_after_story_removal() {
    use designcraft_collab::{CommentAnchor, Operation, SharedId, text::MAX_STORY_ATOMS};

    let mut seed = super::seed();
    let (frame, story) = super::add_text_frame(&mut seed, 0.0);
    let mut author = Replica::new(&seed);
    let selected = seed.story(story).unwrap().text.find("story").unwrap();
    let anchor = author.text_comment_anchor(&seed, story, selected..selected + "story".len()).unwrap();
    assert!(matches!(anchor, CommentAnchor::AutomergeText { .. }));
    let comment_id = super::id("alice", 1);
    let comment = Operation::CreateComment { id: comment_id.clone(), anchor: anchor.clone(), text: "Review the ending".into() };
    let comment: Operation = serde_json::from_slice(&serde_json::to_vec(&comment).unwrap()).unwrap();

    // A comment accepted before the first text change must initialize the same seed as a peer
    // independently drafting that change. Late replay will see the same original selection.
    let mut amended = seed.clone();
    amended.story_mut(story).unwrap().insert(0, "Chapter: ");
    let text = author.capture_automerge_text(&seed, &amended, &BOB, None).unwrap().unwrap();
    let mut reviewer = Replica::new(&seed);
    let mut book = seed.clone();
    reviewer.apply(&mut book, std::slice::from_ref(&comment)).unwrap();
    reviewer.apply_automerge_text(&mut book, &BOB, &text).unwrap();

    let mut late = Replica::new(&seed);
    let mut joined = seed.clone();
    late.apply_automerge_text(&mut joined, &BOB, &text).unwrap();
    late.apply(&mut joined, &[comment]).unwrap();
    for (native, replica) in [(&book, &reviewer), (&joined, &late)] {
        let (_, moved) = replica.comment_text_range(native, &anchor).unwrap();
        assert_eq!(&native.story(story).unwrap().text[moved], "story");
        assert!(!replica.comment_is_orphaned(native, replica.comments().next().unwrap()));
    }

    // The story can now be longer than the legacy text replica supports. Deleting its only
    // frame must still remove the story without deleting its comment.
    let before = book.clone();
    let at = book.story(story).unwrap().len();
    let addition = "Paragraph for the next proof.\n".repeat(MAX_STORY_ATOMS / 20);
    book.story_mut(story).unwrap().insert(at, &addition);
    let long_edit = reviewer.capture_automerge_text(&before, &book, &BOB, None).unwrap().unwrap();
    late.apply_automerge_text(&mut joined, &BOB, &long_edit).unwrap();
    assert!(joined.story(story).unwrap().text.chars().count() > MAX_STORY_ATOMS);
    let spread = joined.spreads[0].id;
    late.apply(&mut joined, &[Operation::DeleteText { id: SharedId::Seed(frame), spread, story }]).unwrap();
    assert!(joined.story(story).is_none());
    assert!(late.comment_text_range(&joined, &anchor).is_none());
    let thread = late.comments().next().unwrap();
    assert_eq!(thread.id, comment_id);
    assert!(late.comment_is_orphaned(&joined, thread));
    assert_eq!(thread.text, "Review the ending");
}
