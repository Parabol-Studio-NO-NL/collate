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
