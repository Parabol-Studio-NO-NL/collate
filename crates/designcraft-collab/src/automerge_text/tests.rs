use designcraft_doc::{Align, ItemId, ParaFormat};

use super::*;

const ALICE: &[u8] = b"alice";
const BOB: &[u8] = b"bob";

fn seeded(text: &str) -> Story {
    let mut story = Story::with_text(StoryId(3), text, ParaFormat::default());
    story.frames.extend([ItemId(4), ItemId(5)]);
    story
}

fn change(replica: &mut AutomergeText, before: &Story, after: &Story, styles: &Styles) -> Vec<u8> {
    replica.capture(before, after, styles, None).unwrap().unwrap()
}

#[test]
fn offline_inserts_merge_both_ways_and_checkpoint_keeps_cursor_identity() {
    let seed = seeded("AéB");
    let styles = Styles::default();
    let mut alice = AutomergeText::seed(&seed, &styles, ALICE).unwrap();
    let mut bob = AutomergeText::seed(&seed, &styles, BOB).unwrap();
    let cursor = alice.cursor("Aé".len()).unwrap();
    assert!(alice.cursor(2).is_err());
    let mut a = seed.clone();
    a.insert(1, "👩‍🎨");
    let mut b = seed.clone();
    b.insert(1, "漢");
    let left = change(&mut alice, &seed, &a, &styles);
    let right = change(&mut bob, &seed, &b, &styles);

    alice.apply_changes(BOB, &right, &styles).unwrap();
    bob.apply_changes(ALICE, &left, &styles).unwrap();
    // A retry after a lost acknowledgment leaves the same state.
    bob.apply_changes(ALICE, &left, &styles).unwrap();
    let mut projected = seed.clone();
    alice.project(&mut projected, &styles).unwrap();
    let mut other = seed.clone();
    bob.project(&mut other, &styles).unwrap();
    assert_eq!(projected, other);
    assert_eq!(projected.frames, seed.frames);
    assert!(projected.text.contains("👩‍🎨"));
    assert!(projected.text.contains("漢"));
    assert_eq!(&projected.text[alice.resolve(&cursor).unwrap()..], "B");
    let mut restored = AutomergeText::load(seed.id, ALICE, &alice.save().unwrap(), &styles).unwrap();
    assert_eq!(restored.resolve(&cursor), bob.resolve(&cursor));
    let mut final_edit = other.clone();
    final_edit.insert(final_edit.len(), "!");
    let after_restart = change(&mut restored, &other, &final_edit, &styles);
    bob.apply_changes(ALICE, &after_restart, &styles).unwrap();
    bob.project(&mut other, &styles).unwrap();
    assert_eq!(other, final_edit);
}

#[test]
fn concurrent_character_and_paragraph_properties_keep_independent_edits() {
    let seed = seeded("Résumé\n文");
    let mut a = seed.clone();
    a.format_chars(0..seed.len(), |format| format.over.size = Some(18.0));
    a.paras[0].para.align = Some(Align::Center);
    a.paras[1].para.space_before = Some(12.0);
    let mut b = seed.clone();
    b.format_chars(0..seed.len(), |format| format.over.underline = Some(true));
    b.paras[0].para.hyphenate = Some(false);
    b.paras[1].para.space_after = Some(7.0);

    let styles = Styles::default();
    let mut alice = AutomergeText::seed(&seed, &styles, ALICE).unwrap();
    let mut bob = AutomergeText::seed(&seed, &styles, BOB).unwrap();
    let left = change(&mut alice, &seed, &a, &styles);
    let right = change(&mut bob, &seed, &b, &styles);
    alice.apply_changes(BOB, &right, &styles).unwrap();
    bob.apply_changes(ALICE, &left, &styles).unwrap();

    let mut projected = seed.clone();
    alice.project(&mut projected, &styles).unwrap();
    bob.project(&mut b, &styles).unwrap();
    b.rev = projected.rev;
    assert_eq!(projected, b);
    assert_eq!(projected.char_format_of(0).over.size, Some(18.0));
    assert_eq!(projected.char_format_of(0).over.underline, Some(true));
    assert_eq!(projected.paras[0].para.align, Some(Align::Center));
    assert_eq!(projected.paras[0].para.hyphenate, Some(false));
    assert_eq!(projected.paras[1].para.space_before, Some(12.0));
    assert_eq!(projected.paras[1].para.space_after, Some(7.0));

    let mut cleared = projected.clone();
    cleared.format_chars(0..2, |format| format.over.underline = None);
    cleared.paras[0].para.align = None;
    cleared.paras[1].para.space_after = None;
    let delta = change(&mut alice, &projected, &cleared, &styles);
    bob.apply_changes(ALICE, &delta, &styles).unwrap();
    bob.project(&mut projected, &styles).unwrap();
    assert_eq!(projected, cleared);
}

#[test]
fn wrong_actor_story_and_out_of_order_change_are_atomic() {
    let seed = seeded("same words");
    let mut another = seed.clone();
    another.id = StoryId(7);
    let styles = Styles::default();
    let mut alice = AutomergeText::seed(&seed, &styles, ALICE).unwrap();
    let mut bob = AutomergeText::seed(&seed, &styles, BOB).unwrap();
    let mut another_replica = AutomergeText::seed(&another, &styles, BOB).unwrap();
    let mut first = seed.clone();
    first.insert(0, "first ");
    let mut second = first.clone();
    second.insert(0, "second ");
    let first_delta = change(&mut alice, &seed, &first, &styles);
    let second_delta = change(&mut alice, &first, &second, &styles);
    assert!(bob.apply_changes(BOB, &first_delta, &styles).is_err());
    assert!(another_replica.apply_changes(ALICE, &first_delta, &styles).is_err());
    assert!(bob.apply_changes(ALICE, &second_delta, &styles).is_err());
    let mut projected = seed.clone();
    bob.project(&mut projected, &styles).unwrap();
    assert_eq!(projected, seed);
    bob.apply_changes(ALICE, &first_delta, &styles).unwrap();
    bob.apply_changes(ALICE, &second_delta, &styles).unwrap();
    bob.project(&mut projected, &styles).unwrap();
    assert_eq!(projected.text, second.text);
}

#[test]
fn formatted_multiline_paste_can_merge_with_typing_in_the_receiving_story() {
    let mut source = Story::with_text(StoryId(17), "Copied\ncaption", ParaFormat::default());
    source.format_chars(0.."Copied".len(), |format| format.over.tracking = Some(25.0));
    source.paras[1].para.align = Some(Align::Right);
    let clipping = source.extract(0..source.len());
    let seed = seeded("Lead:");
    let mut pasted = seed.clone();
    pasted.insert_story(seed.len(), &clipping);
    let mut typed = seed.clone();
    typed.insert(0, "A ");
    let styles = Styles::default();
    let mut alice = AutomergeText::seed(&seed, &styles, ALICE).unwrap();
    let mut bob = AutomergeText::seed(&seed, &styles, BOB).unwrap();

    let paste = change(&mut alice, &seed, &pasted, &styles);
    let typing = change(&mut bob, &seed, &typed, &styles);
    alice.apply_changes(BOB, &typing, &styles).unwrap();
    bob.apply_changes(ALICE, &paste, &styles).unwrap();

    let mut left = seed.clone();
    let mut right = seed.clone();
    alice.project(&mut left, &styles).unwrap();
    bob.project(&mut right, &styles).unwrap();
    assert_eq!(left, right);
    assert_eq!(left.text, "A Lead:Copied\ncaption");
    let range = left.text.find("Copied").unwrap();
    assert_eq!(left.char_format_of(range).over.tracking, Some(25.0));
    assert_eq!(left.paras[1].para.align, Some(Align::Right));

    // Deleting the newline joins the paragraphs and drops the old second paragraph's format.
    let before = left.clone();
    let newline = left.text.find('\n').unwrap();
    left.delete(newline..newline + 1);
    let joined = change(&mut alice, &before, &left, &styles);
    bob.apply_changes(ALICE, &joined, &styles).unwrap();
    bob.project(&mut right, &styles).unwrap();
    assert_eq!(left, right);
    assert_eq!(right.paras, vec![ParaFormat::default()]);
}

#[test]
fn a_change_in_a_long_threaded_story_sends_the_edit_instead_of_the_chapter() {
    let seed = seeded(&"A chapter in the working proof. The next line belongs to the same thread.\n".repeat(512));
    let mut revised = seed.clone();
    let position = revised.text.find("next").unwrap();
    revised.insert(position, "author's ");
    let styles = Styles::default();
    let mut writer = AutomergeText::seed(&seed, &styles, ALICE).unwrap();
    let mut reader = AutomergeText::seed(&seed, &styles, BOB).unwrap();

    let delta = change(&mut writer, &seed, &revised, &styles);
    assert!(delta.len() < seed.len(), "a single insertion must travel independently of its story size");
    reader.apply_changes(ALICE, &delta, &styles).unwrap();
    let mut projected = seed;
    reader.project(&mut projected, &styles).unwrap();
    assert_eq!(projected, revised);
}

#[test]
fn a_comment_tracks_only_surviving_original_characters_after_merge_and_checkpoint() {
    let seed = seeded("Aé😊B");
    let styles = Styles::default();
    let mut alice = AutomergeText::seed(&seed, &styles, ALICE).unwrap();
    let mut bob = AutomergeText::seed(&seed, &styles, BOB).unwrap();
    let anchor = alice.comment_anchor(1.."Aé😊".len()).unwrap();
    assert_eq!(alice.comment_range(&anchor), Some(1.."Aé😊".len()));
    let encoded = serde_json::to_vec(&anchor).unwrap();
    let anchor: AutomergeCommentAnchor = serde_json::from_slice(&encoded).unwrap();

    let mut inserted = seed.clone();
    inserted.insert("Aé".len(), "between");
    let insertion = change(&mut alice, &seed, &inserted, &styles);
    let mut shortened = seed.clone();
    shortened.delete(1.."Aé".len());
    let deletion = change(&mut bob, &seed, &shortened, &styles);
    let range = alice.comment_range(&anchor).unwrap();
    assert_eq!(&inserted.text[range], "ébetween😊");

    alice.apply_changes(BOB, &deletion, &styles).unwrap();
    bob.apply_changes(ALICE, &insertion, &styles).unwrap();
    bob.validate_comment_anchor(&anchor).unwrap();
    let mut merged = seed.clone();
    alice.project(&mut merged, &styles).unwrap();
    let remaining = "Abetween".len().."Abetween😊".len();
    assert_eq!(alice.comment_range(&anchor), Some(remaining.clone()));
    assert_eq!(alice.comment_range(&anchor), bob.comment_range(&anchor));

    let current = alice.comment_anchor(1.."Abetween😊".len()).unwrap();
    let bytes = alice.save().unwrap();
    let mut restored = AutomergeText::load(seed.id, ALICE, &bytes, &styles).unwrap();
    assert_eq!(restored.comment_range(&anchor), Some(remaining.clone()));
    // Equal bytes are not equal characters after a replacement.
    let mut replaced = merged.clone();
    replaced.replace(remaining.clone(), "😊");
    let replacement = restored.capture(&merged, &replaced, &styles, Some(TextSplice { old: remaining.clone(), new: remaining })).unwrap().unwrap();
    bob.apply_changes(ALICE, &replacement, &styles).unwrap();
    bob.validate_comment_anchor(&anchor).unwrap();
    assert_eq!(restored.comment_range(&anchor), None);
    assert_eq!(bob.comment_range(&anchor), None);
    let survived = bob.comment_range(&current).unwrap();
    assert_eq!(&merged.text[survived], "between");
}

#[test]
fn comment_anchors_require_exact_boundaries_known_history_and_bounded_wire_data() {
    let source = "é".repeat(crate::text::MAX_EDIT_ATOMS + 1);
    let mut seed = seeded(&source);
    let styles = Styles::default();
    let original = AutomergeText::seed(&seed, &styles, ALICE).unwrap();
    let anchor = original.comment_anchor(0..source.len() - "é".len()).unwrap();
    assert!(original.comment_anchor(0..source.len()).is_err());
    assert!(original.comment_anchor(0..1).is_err());
    assert!(original.comment_anchor(1..2).is_err());
    assert!(original.comment_anchor(0..0).is_err());
    seed.id = StoryId(seed.id.0 + 1);
    let unrelated = AutomergeText::seed(&seed, &styles, BOB).unwrap();
    assert!(unrelated.validate_comment_anchor(&anchor).is_err());
    assert_eq!(unrelated.comment_range(&anchor), None);

    let encoded = serde_json::to_vec(&anchor).unwrap();
    assert!(encoded.len() < source.len());
    let oversized = serde_json::json!({"heads": "A".repeat(4096), "start": 0, "end": 2});
    assert!(serde_json::from_value::<AutomergeCommentAnchor>(oversized).is_err());
}
