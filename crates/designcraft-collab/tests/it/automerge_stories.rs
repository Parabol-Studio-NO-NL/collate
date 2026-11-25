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
    assert!(matches!(anchor, CommentAnchor::ReversibleText { .. }));
    let comment_id = super::id("alice", 1);
    let comment = Operation::CreateComment { id: comment_id.clone(), anchor: anchor.clone(), text: "Review the ending".into() };
    let comment: Operation = serde_json::from_slice(&serde_json::to_vec(&comment).unwrap()).unwrap();

    // A comment accepted before the first text change must initialize the same seed as a peer
    // independently drafting that change. Late replay will see the same original selection.
    let mut amended = seed.clone();
    amended.story_mut(story).unwrap().insert(0, "Chapter: ");
    let text = author.capture_reversible_text(&seed, &amended, &BOB, 2, None).unwrap().unwrap();
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
    let long_edit = reviewer.capture_reversible_text(&before, &book, &BOB, 3, None).unwrap().unwrap();
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

#[cfg(feature = "automerge-rooms")]
mod reversible {
    use super::*;
    use designcraft_collab::{CommentAnchor, Operation, automerge_text::AutomergeText, text::TextSplice};
    use designcraft_doc::{Align, Document, Story};

    const CAROL: [u8; 32] = [0xc1; 32];

    fn with_text(text: &str) -> (Document, StoryId) {
        let mut seed = super::super::seed();
        let (_, story) = super::super::add_text_frame(&mut seed, 0.0);
        let draft = seed.story_mut(story).unwrap();
        draft.replace(0..draft.len(), text);
        (seed, story)
    }

    fn capture(
        replica: &Replica,
        document: &Document,
        story: StoryId,
        actor: &[u8; 32],
        sequence: u64,
        edit: impl FnOnce(&mut Story) -> Option<TextSplice>,
    ) -> StoryTransaction {
        let mut draft = document.clone();
        let hint = edit(draft.story_mut(story).unwrap()).map(|hint| (story, hint));
        let transaction = replica.clone().capture_reversible_text(document, &draft, actor, sequence, hint).unwrap().unwrap();
        assert_eq!(transaction.reversible_sequence(), Some(sequence));
        serde_json::from_slice(&serde_json::to_vec(&transaction).unwrap()).unwrap()
    }

    fn apply(replica: &mut Replica, document: &mut Document, actor: &[u8; 32], transaction: &StoryTransaction) {
        replica.apply_automerge_text(document, actor, transaction).unwrap();
    }

    fn undo(replica: &Replica, actor: &[u8; 32], sequence: u64, transaction: &StoryTransaction) -> Vec<Operation> {
        let edits = replica.inverse_reversible_text(actor, sequence, transaction).unwrap();
        let [Operation::SetAutomergeTextBatchActive { story, actor, original_sequence, active: false }] = edits.as_slice() else {
            panic!("expected author toggle")
        };
        assert!(replica.can_activate_automerge_text_batch(*story, actor, *original_sequence, false));
        edits
    }

    fn toggle(replica: &mut Replica, document: &mut Document, operations: &[Operation]) -> Vec<Operation> {
        let inverse = replica.inverse_patch_batch(document, operations).unwrap().unwrap();
        replica.apply(document, operations).unwrap();
        inverse
    }

    #[test]
    fn undo_hides_only_the_original_birth_and_revives_its_comment_on_redo() {
        let (mut document, story) = with_text("AB");
        let seed = document.clone();
        let mut replica = Replica::new(&document);
        let alice = capture(&replica, &document, story, &ALICE, 1, |story| {
            story.insert(1, "λ");
            None
        });
        apply(&mut replica, &mut document, &ALICE, &alice);
        let anchor = replica.text_comment_anchor(&document, story, 1.."Aλ".len()).unwrap();
        let comment = Operation::CreateComment { id: super::super::id("alice", 2), anchor: anchor.clone(), text: "Check this letter".into() };
        replica.apply(&mut document, std::slice::from_ref(&comment)).unwrap();

        let bob = capture(&replica, &document, story, &BOB, 1, |story| {
            story.insert("Aλ".len(), "文");
            None
        });
        apply(&mut replica, &mut document, &BOB, &bob);
        let caret = replica.automerge_cursor(story, "Aλ文".len()).unwrap();
        let reversal = undo(&replica, &ALICE, 1, &alice);
        assert!(!reversal[0].is_authored_by(&automerge::ActorId::from(BOB.as_slice()).to_string()));
        let redo = toggle(&mut replica, &mut document, &reversal);
        assert_eq!(document.story(story).unwrap().text, "A文B");
        assert_eq!(replica.resolve_automerge_cursor(story, &caret), Some("A文".len()));
        assert!(replica.comment_is_orphaned(&document, replica.comments().next().unwrap()));
        assert!(replica.apply(&mut document, &reversal).is_err());

        toggle(&mut replica, &mut document, &redo);
        assert_eq!(document.story(story).unwrap().text, "Aλ文B");
        assert_eq!(replica.comment_text_range(&document, &anchor), Some((story, 1.."Aλ".len())));

        let mut replay = Replica::new(&seed);
        let mut reopened = seed;
        apply(&mut replay, &mut reopened, &ALICE, &alice);
        replay.apply(&mut reopened, &[comment]).unwrap();
        apply(&mut replay, &mut reopened, &BOB, &bob);
        toggle(&mut replay, &mut reopened, &reversal);
        toggle(&mut replay, &mut reopened, &redo);
        assert_eq!(reopened, document);
        assert_eq!(replay.comment_text_range(&reopened, &anchor), replica.comment_text_range(&document, &anchor));
    }

    #[test]
    fn a_caret_stays_behind_its_original_character_when_a_peer_inserts_in_the_gap() {
        let (mut document, story) = with_text("AB");
        let mut replica = Replica::new(&document);
        let first = capture(&replica, &document, story, &ALICE, 1, |story| {
            story.insert(1, "Ω");
            None
        });
        apply(&mut replica, &mut document, &ALICE, &first);
        let start = replica.automerge_cursor(story, 0).unwrap();
        let original_end = "AΩ".len();
        let caret = replica.automerge_cursor(story, original_end).unwrap();
        let second = capture(&replica, &document, story, &BOB, 1, |story| {
            story.insert(original_end, "λ");
            None
        });
        apply(&mut replica, &mut document, &BOB, &second);
        assert_eq!(document.story(story).unwrap().text, "AΩλB");
        assert_eq!(replica.resolve_automerge_cursor(story, &start), Some(0));
        assert_eq!(replica.resolve_automerge_cursor(story, &caret), Some(original_end));
        let undo = undo(&replica, &ALICE, 1, &first);
        toggle(&mut replica, &mut document, &undo);
        assert_eq!(document.story(story).unwrap().text, "AλB");
        assert_eq!(replica.resolve_automerge_cursor(story, &caret), Some(1));
    }

    #[test]
    fn concurrent_delete_tags_and_insertions_replay_in_both_orders() {
        let (seed, story) = with_text("ABCD");
        let author = Replica::new(&seed);
        let alice = capture(&author, &seed, story, &ALICE, 1, |story| {
            story.delete(1..3);
            None
        });
        let bob = capture(&author, &seed, story, &BOB, 1, |story| {
            story.delete(1..2);
            None
        });
        let carol = capture(&author, &seed, story, &CAROL, 1, |story| {
            story.insert(3, "文");
            None
        });
        for commits in [[(&ALICE, &alice), (&BOB, &bob), (&CAROL, &carol)], [(&CAROL, &carol), (&BOB, &bob), (&ALICE, &alice)]] {
            let mut book = seed.clone();
            let mut host = Replica::new(&book);
            for (actor, transaction) in commits {
                apply(&mut host, &mut book, actor, transaction);
            }
            assert_eq!(book.story(story).unwrap().text, "A文D");
            let undo_a = undo(&host, &ALICE, 1, &alice);
            let undo_b = undo(&host, &BOB, 1, &bob);
            let redo_a = toggle(&mut host, &mut book, &undo_a);
            assert_eq!(book.story(story).unwrap().text, "AC文D");
            let redo_b = toggle(&mut host, &mut book, &undo_b);
            assert_eq!(book.story(story).unwrap().text, "ABC文D");
            toggle(&mut host, &mut book, &redo_b);
            assert_eq!(book.story(story).unwrap().text, "AC文D");
            toggle(&mut host, &mut book, &redo_a);
            assert_eq!(book.story(story).unwrap().text, "A文D");
        }
    }

    #[test]
    fn format_redo_keeps_original_rank_and_paragraph_format_follows_the_restored_break() {
        let (mut document, id) = with_text("A\nB");
        let mut room = Replica::new(&document);
        let alice = capture(&room, &document, id, &ALICE, 1, |story| {
            story.format_chars(0..1, |format| format.over.size = Some(18.0));
            story.paras[1].para.align = Some(Align::Right);
            None
        });
        apply(&mut room, &mut document, &ALICE, &alice);
        let bob = capture(&room, &document, id, &BOB, 1, |story| {
            story.format_chars(0..1, |format| format.over.size = Some(24.0));
            None
        });
        let carol = capture(&room, &document, id, &CAROL, 1, |story| {
            story.format_chars(0..1, |format| format.over.underline = Some(true));
            story.delete(1..2);
            None
        });
        apply(&mut room, &mut document, &CAROL, &carol);
        apply(&mut room, &mut document, &BOB, &bob);
        let undo_a = undo(&room, &ALICE, 1, &alice);
        let redo_a = toggle(&mut room, &mut document, &undo_a);
        toggle(&mut room, &mut document, &redo_a);
        assert_eq!(document.story(id).unwrap().chars[0].format.over.size, Some(24.0));
        let undo_b = undo(&room, &BOB, 1, &bob);
        toggle(&mut room, &mut document, &undo_b);
        let story = document.story(id).unwrap();
        assert_eq!(story.chars[0].format.over.size, Some(18.0));
        assert_eq!(story.chars[0].format.over.underline, Some(true));
        assert_eq!(story.paras.len(), 1);
        let undo_c = undo(&room, &CAROL, 1, &carol);
        toggle(&mut room, &mut document, &undo_c);
        let story = document.story(id).unwrap();
        assert_eq!(story.text, "A\nB");
        assert_eq!(story.paras[1].para.align, Some(Align::Right));
        assert_eq!(story.chars[0].format.over.underline, None);
    }

    #[test]
    fn protocol_16_text_and_comment_records_remain_readable_without_reinterpreting_ids() {
        let (mut document, id) = with_text("AB");
        let native = document.story(id).unwrap();
        let old = AutomergeText::seed(native, &document.styles, &ALICE).unwrap();
        let anchor = CommentAnchor::AutomergeText { story: id, anchor: old.comment_anchor(1..2).unwrap() };
        let comment = Operation::CreateComment { id: super::super::id("alice", 1), anchor: anchor.clone(), text: "Legacy comment".into() };
        let mut room = Replica::new(&document);
        room.apply(&mut document, &[comment]).unwrap();
        let mut draft = document.clone();
        draft.story_mut(id).unwrap().insert(0, "λ");
        let action = room.clone().capture_reversible_text(&document, &draft, &BOB, 1, None).unwrap().unwrap();
        assert_eq!(action.reversible_sequence(), None);
        assert!(room.inverse_reversible_text(&BOB, 1, &action).is_none());
        apply(&mut room, &mut document, &BOB, &action);
        let (_, range) = room.comment_text_range(&document, &anchor).unwrap();
        assert_eq!(&document.story(id).unwrap().text[range], "B");
    }
}
