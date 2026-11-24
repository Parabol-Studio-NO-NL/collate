use designcraft_doc::{Align, CharFormat, ItemId, ParaFormat, anchored::OBJECT_MARK, styles::ParagraphStyle};

use super::{
    AtomId, BasicTextFormat, Error, MAX_EDIT_ATOMS, ParagraphFormatChange, Story, StoryId, Styles, TextEdit, TextFormatChange, TextReplica,
    TextSplice,
};

fn story(value: &str) -> Story {
    let mut story = Story::with_text(StoryId(1), value, ParaFormat::default());
    story.frames.extend([ItemId(2), ItemId(3)]);
    story
}

#[test]
fn stale_ranges_preserve_insertions_and_independent_formats_in_host_order() {
    let styles = Styles::default();
    let seed = story("Aé😊B");
    let stale = TextReplica::new(&seed, &styles).unwrap();
    let old_range = stale.atoms_in(1..7).unwrap();
    let old_anchor = stale.atom_before(7).unwrap();
    let insert = TextEdit::Insert { after: stale.atom_before(3).unwrap(), text: "\n文".into(), format: BasicTextFormat::default() };
    let insert: TextEdit = serde_json::from_str(&serde_json::to_string(&insert).unwrap()).unwrap();
    let bold_size = TextEdit::Format { atoms: old_range.clone(), change: TextFormatChange::Size(Some(18.0)) };
    let underline = TextEdit::Format { atoms: old_range.clone(), change: TextFormatChange::Underline(Some(true)) };
    let mut host = seed.clone();
    let mut host_replica = stale.clone();
    host_replica.apply(&mut host, &styles, "alice", 1, std::slice::from_ref(&insert)).unwrap();
    host_replica.apply(&mut host, &styles, "bob", 1, std::slice::from_ref(&bold_size)).unwrap();
    host_replica.apply(&mut host, &styles, "carol", 1, std::slice::from_ref(&underline)).unwrap();
    assert_eq!(host.text, "Aé\n文😊B");
    assert_eq!(host.char_format_of(1).over.size, Some(18.0));
    assert_eq!(host.char_format_of(1).over.underline, Some(true));
    assert_eq!(host.char_format_of(4).over, CharFormat::default().over);
    assert_eq!(host.char_format_of(7).over.underline, Some(true));
    host.check().unwrap();

    let delete = TextEdit::Delete { atoms: old_range };
    let anchored = TextEdit::Insert { after: old_anchor, text: "Z".into(), format: BasicTextFormat::default() };
    host_replica.apply(&mut host, &styles, "bob", 2, std::slice::from_ref(&delete)).unwrap();
    host_replica.apply(&mut host, &styles, "alice", 2, std::slice::from_ref(&anchored)).unwrap();
    assert_eq!(host.text, "A\n文ZB");
    assert_eq!(host.frames, seed.frames);
    assert_eq!(host.paras, vec![ParaFormat::default(); 2]);
    host.check().unwrap();

    let ordered = [("alice", 1, insert), ("bob", 1, bold_size), ("carol", 1, underline), ("bob", 2, delete), ("alice", 2, anchored)];
    for _ in 0..2 {
        let mut replayed = seed.clone();
        let mut replica = TextReplica::new(&seed, &styles).unwrap();
        for (actor, seq, edit) in &ordered {
            replica.apply(&mut replayed, &styles, actor, *seq, std::slice::from_ref(edit)).unwrap();
        }
        assert_eq!(host, replayed);
        assert_eq!(host_replica.atoms_in(0..host.text.len()), replica.atoms_in(0..replayed.text.len()));
    }
}

#[test]
fn same_anchor_insertions_keep_both_edits_and_reject_identity_reuse() {
    let styles = Styles::default();
    let mut current = story("AB");
    let mut replica = TextReplica::new(&current, &styles).unwrap();
    let after = replica.atom_before(1).unwrap();
    let alice = TextEdit::Insert { after: after.clone(), text: "red".into(), format: BasicTextFormat::default() };
    let bob = TextEdit::Insert { after, text: "blue".into(), format: BasicTextFormat::default() };
    replica.apply(&mut current, &styles, "alice", 1, std::slice::from_ref(&alice)).unwrap();
    replica.apply(&mut current, &styles, "bob", 1, &[bob]).unwrap();
    assert_eq!(current.text, "AblueredB");
    assert_eq!(replica.apply(&mut current, &styles, "alice", 1, &[alice]), Err(Error::DuplicateId));
    assert_eq!(current.text, "AblueredB");
}

#[test]
fn undoing_an_insertion_preserves_another_authors_insertions_at_its_anchor() {
    let styles = Styles::default();
    let mut current = story("AB");
    let mut replica = TextReplica::new(&current, &styles).unwrap();
    let put = |after, text: &str| TextEdit::Insert { after: Some(after), text: text.into(), format: BasicTextFormat::default() };
    replica.apply(&mut current, &styles, "alice", 1, &[put(AtomId::Seed(0), "λ")]).unwrap();
    let original = replica.atom_before(3).unwrap().unwrap();
    replica.apply(&mut current, &styles, "bob", 1, &[put(original.clone(), "文")]).unwrap();
    let introduced = replica.atom_before(6).unwrap().unwrap();

    assert!(replica.can_set_batch_active("alice", 1, false));
    replica.set_batch_active(&mut current, "alice", 1, false).unwrap();
    assert_eq!(current.text, "A文B");
    assert_eq!(replica.byte_after(&original), Ok(1));
    assert_eq!(replica.atoms_in(1..4).unwrap(), std::slice::from_ref(&introduced));
    assert!(!replica.can_set_batch_active("alice", 1, false));
    assert_eq!(replica.set_batch_active(&mut current, "alice", 1, false), Err(Error::Invalid("text history state")));

    replica.apply(&mut current, &styles, "carol", 1, &[put(original.clone(), "é")]).unwrap();
    replica.set_batch_active(&mut current, "alice", 1, true).unwrap();
    assert_eq!(current.text, "Aλé文B");
    assert_eq!(replica.byte_after(&original), Ok(3));
    assert_eq!(replica.atoms_in(5..8).unwrap(), [introduced]);
}

#[test]
fn overlapping_deletes_are_undone_and_redone_independently() {
    let styles = Styles::default();
    let mut current = story("ABCD");
    let mut replica = TextReplica::new(&current, &styles).unwrap();
    let delete = |ids: &[u32]| TextEdit::Delete { atoms: ids.iter().copied().map(AtomId::Seed).collect() };
    replica.apply(&mut current, &styles, "alice", 1, &[delete(&[1, 2])]).unwrap();
    replica.apply(&mut current, &styles, "bob", 1, &[delete(&[1])]).unwrap();
    let insert = TextEdit::Insert { after: Some(AtomId::Seed(2)), text: "文".into(), format: BasicTextFormat::default() };
    replica.apply(&mut current, &styles, "carol", 1, &[insert]).unwrap();
    assert_eq!(current.text, "A文D");

    replica.set_batch_active(&mut current, "alice", 1, false).unwrap();
    assert_eq!(current.text, "AC文D");
    assert_eq!(replica.atoms_in(1..2).unwrap(), [AtomId::Seed(2)]);
    replica.set_batch_active(&mut current, "bob", 1, false).unwrap();
    assert_eq!(current.text, "ABC文D");
    replica.set_batch_active(&mut current, "bob", 1, true).unwrap();
    assert_eq!(current.text, "AC文D");
    replica.set_batch_active(&mut current, "alice", 1, true).unwrap();
    assert_eq!(current.text, "A文D");
}

#[test]
fn redoing_a_format_restores_its_original_order_and_leaves_other_attributes() {
    let styles = Styles::default();
    let mut current = story("A");
    let mut replica = TextReplica::new(&current, &styles).unwrap();
    let format = |change| TextEdit::Format { atoms: vec![AtomId::Seed(0)], change };
    replica.apply(&mut current, &styles, "alice", 1, &[format(TextFormatChange::Size(Some(18.0)))]).unwrap();
    replica.apply(&mut current, &styles, "bob", 1, &[format(TextFormatChange::Size(Some(24.0)))]).unwrap();
    replica.apply(&mut current, &styles, "carol", 1, &[format(TextFormatChange::Underline(Some(true)))]).unwrap();

    replica.set_batch_active(&mut current, "alice", 1, false).unwrap();
    assert_eq!(current.char_format_of(0).over.size, Some(24.0));
    replica.set_batch_active(&mut current, "alice", 1, true).unwrap();
    assert_eq!(current.char_format_of(0).over.size, Some(24.0));
    replica.set_batch_active(&mut current, "bob", 1, false).unwrap();
    assert_eq!(current.char_format_of(0).over.size, Some(18.0));
    assert_eq!(current.char_format_of(0).over.underline, Some(true));
    replica.set_batch_active(&mut current, "bob", 1, true).unwrap();
    assert_eq!(current.char_format_of(0).over.size, Some(24.0));
    assert_eq!(current.char_format_of(0).over.underline, Some(true));
}

#[test]
fn empty_typing_format_uses_the_original_style_and_the_active_delete_sample() {
    let styles = Styles::default();
    let mut current = story("");
    current.chars[0].format.over.size = Some(22.0);
    let mut replica = TextReplica::new(&current, &styles).unwrap();
    let insert = TextEdit::Insert { after: None, text: "é".into(), format: BasicTextFormat::default() };
    replica.apply(&mut current, &styles, "alice", 1, &[insert]).unwrap();
    replica.set_batch_active(&mut current, "alice", 1, false).unwrap();
    assert!(current.text.is_empty());
    assert_eq!(current.chars[0].format.over.size, Some(22.0));
    replica.set_batch_active(&mut current, "alice", 1, true).unwrap();
    let atoms = replica.atoms_in(0..2).unwrap();
    let format = |size| TextEdit::Format { atoms: atoms.clone(), change: TextFormatChange::Size(Some(size)) };
    replica.apply(&mut current, &styles, "bob", 1, &[format(18.0), TextEdit::Delete { atoms: atoms.clone() }]).unwrap();
    assert_eq!(current.chars[0].format.over.size, Some(18.0));
    let empty = current.clone();
    replica.apply(&mut current, &styles, "carol", 1, &[format(24.0)]).unwrap();
    assert_eq!(current, empty, "formatting a hidden atom has no effect in ordinary version 6 playback");
    replica.set_batch_active(&mut current, "bob", 1, false).unwrap();
    assert_eq!(current.text, "é");
    assert_eq!(current.char_format_of(0).over.size, None);
    replica.set_batch_active(&mut current, "bob", 1, true).unwrap();
    assert!(current.text.is_empty());
    assert_eq!(current.chars[0].format.over.size, Some(18.0));
}

#[test]
fn invalid_or_unsupported_story_changes_leave_the_replica_and_story_unchanged() {
    let styles = Styles::default();
    let mut current = story("é");
    let mut replica = TextReplica::new(&current, &styles).unwrap();
    assert_eq!(replica.atom_before(1), Err(Error::Invalid("text byte offset")));
    let mut malformed = current.clone();
    malformed.chars[0].len = 1;
    malformed.chars.push(malformed.chars[0].clone());
    assert!(matches!(TextReplica::new(&malformed, &styles), Err(Error::Invalid("story run boundaries"))));
    let anchored_story = story(&format!("a{OBJECT_MARK}"));
    assert!(matches!(TextReplica::new(&anchored_story, &styles), Err(Error::UnsupportedEdit)));

    let original = current.clone();
    let insert = TextEdit::Insert { after: None, text: "x".into(), format: BasicTextFormat::default() };
    let invalid = TextEdit::Delete { atoms: vec![AtomId::Seed(99)] };
    assert_eq!(replica.apply(&mut current, &styles, "alice", 1, &[insert.clone(), invalid]), Err(Error::Invalid("text target")));
    assert_eq!(current, original);
    let large = TextEdit::Insert { after: None, text: "x".repeat(MAX_EDIT_ATOMS + 1), format: BasicTextFormat::default() };
    assert_eq!(replica.apply(&mut current, &styles, "alice", 1, &[large]), Err(Error::Limit));
    assert_eq!(current, original);
    assert_eq!(replica.apply(&mut malformed, &styles, "alice", 1, std::slice::from_ref(&insert)), Err(Error::Invalid("story projection")));
    for change in [
        ParagraphFormatChange::Language(Some("a".repeat(256))),
        ParagraphFormatChange::Language(Some("bad\nname".into())),
        ParagraphFormatChange::RagLineInsets(Some([0.0, 2.0, -1.0, 3.0])),
        ParagraphFormatChange::RagNearThreshold(Some(designcraft_doc::MAX_MEASURE + 1.0)),
    ] {
        let bad = TextEdit::FormatParagraph { after: None, change };
        assert!(replica.apply(&mut current, &styles, "alice", 1, &[insert.clone(), bad]).is_err());
        assert_eq!(current, original);
    }
    replica.apply(&mut current, &styles, "alice", 1, &[insert]).unwrap();
    assert_eq!(current.text, "xé");
}

#[test]
fn empty_projection_preserves_typing_format_and_deleted_anchors() {
    let styles = Styles::default();
    let mut current = story("é");
    current.format_chars(0..2, |fmt| fmt.over.size = Some(22.0));
    let mut replica = TextReplica::new(&current, &styles).unwrap();
    let after = replica.atom_before(2).unwrap();
    let atoms = replica.atoms_in(0..2).unwrap();
    replica.apply(&mut current, &styles, "alice", 1, &[TextEdit::Delete { atoms }]).unwrap();
    assert!(current.text.is_empty());
    assert_eq!(current.chars[0].format.over.size, Some(22.0));
    current.check().unwrap();

    let insert = TextEdit::Insert { after, text: "x".into(), format: BasicTextFormat::from_char_format(&current.chars[0].format).unwrap() };
    replica.apply(&mut current, &styles, "bob", 1, &[insert]).unwrap();
    assert_eq!(current.text, "x");
    assert_eq!(current.chars[0].format.over.size, Some(22.0));
    current.check().unwrap();
}

#[test]
fn capture_reproduces_a_unicode_splice_and_exact_formatting_delta() {
    let styles = Styles::default();
    let mut before = story("aé😊z");
    before.format_chars(0..1, |format| format.over.underline = Some(true));
    let mut after = before.clone();
    after.replace(1..3, "文x");
    after.format_chars(0..1, |format| format.over.underline = None);
    after.format_chars(1..4, |format| format.over.size = Some(18.0));
    let mut replica = TextReplica::new(&before, &styles).unwrap();
    let removed = replica.atoms_in(1..3).unwrap();
    let kept = replica.atoms_in(3..7).unwrap();
    let edits = replica.capture(&before, &after, &styles, "alice", 1).unwrap();
    assert_eq!(edits.iter().map(TextEdit::inserted_atom_count).sum::<usize>(), 2);
    assert!(matches!(&edits[0], TextEdit::Delete { atoms } if *atoms == removed));
    assert!(edits.iter().any(|op| matches!(op, TextEdit::Format { change: TextFormatChange::Underline(None), .. })));

    let mut projected = before.clone();
    replica.apply(&mut projected, &styles, "alice", 1, &edits).unwrap();
    assert_eq!(replica.atoms_in(5..9).unwrap(), kept);
    let caret = AtomId::Inserted { actor: "alice".into(), sequence: 1, index: 1 };
    assert_eq!(replica.byte_after(&caret), Ok(5));
    projected.rev = after.rev;
    assert_eq!(projected, after);

    let mut unsupported = after;
    unsupported.frames.reverse();
    assert_eq!(replica.capture(&projected, &unsupported, &styles, "alice", 2), Err(Error::UnsupportedEdit));
}

#[test]
fn a_caret_hint_retains_the_intended_anchor_among_identical_characters() {
    let styles = Styles::default();
    let before = story("ééé");
    let mut after = before.clone();
    after.insert(2, "é");
    let mut replica = TextReplica::new(&before, &styles).unwrap();
    let exact = TextSplice { old: 2..2, new: 2..4 };
    let edits = replica.capture_at(&before, &after, &styles, "alice", 1, Some(exact)).unwrap();
    assert!(matches!(&edits[..], [TextEdit::Insert { after: Some(AtomId::Seed(0)), .. }]));
    let invalid = TextSplice { old: 1..1, new: 1..3 };
    let fallback = replica.capture_at(&before, &after, &styles, "alice", 1, Some(invalid)).unwrap();
    assert!(matches!(&fallback[..], [TextEdit::Insert { after: Some(AtomId::Seed(2)), .. }]));

    let mut projected = before.clone();
    replica.apply(&mut projected, &styles, "alice", 1, &edits).unwrap();
    assert_eq!(projected, after);
    assert_eq!(replica.byte_after(&AtomId::Seed(1)), Ok(6));
}

#[test]
fn paragraph_formatting_stays_with_a_deleted_break_until_the_break_is_restored() {
    let styles = Styles::default();
    let mut current = story("AB\nCD");
    current.paras[0].para.align = Some(Align::Right);
    let mut replica = TextReplica::new(&current, &styles).unwrap();
    let separator = AtomId::Seed(2);
    let merge = TextEdit::Delete { atoms: vec![separator.clone()] };
    let stale = TextEdit::FormatParagraph { after: Some(separator), change: ParagraphFormatChange::Align(Some(Align::Center)) };
    let stale = serde_json::from_str(&serde_json::to_string(&stale).unwrap()).unwrap();
    replica.apply(&mut current, &styles, "alice", 1, &[merge]).unwrap();
    replica.apply(&mut current, &styles, "bob", 1, &[stale]).unwrap();
    assert_eq!(current.text, "ABCD");
    assert_eq!(current.paras.len(), 1);
    assert_eq!(current.paras[0].para.align, Some(Align::Right));

    replica.set_batch_active(&mut current, "alice", 1, false).unwrap();
    assert_eq!(current.text, "AB\nCD");
    assert_eq!(current.paras[1].para.align, Some(Align::Center));
    replica.set_batch_active(&mut current, "bob", 1, false).unwrap();
    assert_eq!(current.paras[1].para.align, None);

    let before = current.clone();
    let valid = TextEdit::FormatParagraph { after: None, change: ParagraphFormatChange::Align(None) };
    let bad = TextEdit::FormatParagraph { after: Some(AtomId::Seed(0)), change: ParagraphFormatChange::Align(None) };
    assert_eq!(replica.apply(&mut current, &styles, "alice", 2, &[valid, bad]), Err(Error::Invalid("paragraph anchor")));
    assert_eq!(current, before);
    replica.set_batch_active(&mut current, "bob", 1, true).unwrap();
    assert_eq!(current.paras[1].para.align, Some(Align::Center));
}

#[test]
fn capture_formats_a_new_paragraph_by_its_inserted_break() {
    let styles = Styles::default();
    let mut current = story("AB\nCD");
    current.paras[0].para.align = Some(Align::Right);
    current.paras[1].para.align = Some(Align::Center);
    let mut draft = current.clone();
    draft.insert(1, "\n");
    assert_eq!(draft.paras[1].para.align, Some(Align::Right));
    draft.paras[1].para.space_before = Some(8.0);

    let mut replica = TextReplica::new(&current, &styles).unwrap();
    let edits = replica.capture(&current, &draft, &styles, "alice", 1).unwrap();
    assert!(matches!(&edits[..], [
        TextEdit::Insert { text, .. },
        TextEdit::FormatParagraph {
            after: Some(AtomId::Inserted { actor, sequence: 1, index: 0 }),
            change: ParagraphFormatChange::SpaceBefore(Some(8.0)),
        },
    ] if text == "\n" && actor.as_ref() == "alice"));
    replica.apply(&mut current, &styles, "alice", 1, &edits).unwrap();
    assert_eq!(current, draft);
}

#[test]
fn old_text_operations_replay_with_a_long_existing_paragraph_style() {
    let mut styles = Styles::default();
    let name = "Long style name ".repeat(10);
    styles.paragraph.push(ParagraphStyle {
        name: name.clone(),
        based_on: None,
        next_style: None,
        para: Default::default(),
        chars: Default::default(),
        shortcut: String::new(),
    });
    let mut current = story("AB");
    current.paras[0].style.clone_from(&name);
    let mut replica = TextReplica::new(&current, &styles).unwrap();
    let edit = serde_json::from_value(serde_json::json!({
        "type": "insert",
        "after": { "kind": "seed", "value": 0 },
        "text": "\n",
        "format": { "style": "[None]" }
    }))
    .unwrap();
    replica.apply(&mut current, &styles, "alice", 1, &[edit]).unwrap();
    assert_eq!(current.text, "A\nB");
    assert!(current.paras.iter().all(|paragraph| paragraph.style == name));
    let legacy_rag = serde_json::from_value(serde_json::json!({
        "type": "format_paragraph", "after": null, "change": {"kind":"rag_pattern", "value":"alternatingEdges"}
    }))
    .unwrap();
    replica.apply(&mut current, &styles, "alice", 2, &[legacy_rag]).unwrap();
    assert_eq!(current.paras[0].para.rag_pattern, Some(designcraft_doc::RagPattern::AlternatingEdges));
    assert!(current.paras[0].para.rag_line_insets.is_none());
}

#[test]
fn replacing_a_break_with_identical_text_creates_a_new_paragraph_identity() {
    let styles = Styles::default();
    let mut current = story("AB\nCD");
    current.paras[0].para.align = Some(Align::Right);
    current.paras[1].para.align = Some(Align::Center);
    let mut draft = current.clone();
    draft.replace(2..3, "\n");

    let mut replica = TextReplica::new(&current, &styles).unwrap();
    let splice = TextSplice { old: 2..3, new: 2..3 };
    let edits = replica.capture_at(&current, &draft, &styles, "alice", 1, Some(splice)).unwrap();
    assert!(matches!(&edits[..], [
        TextEdit::Delete { atoms },
        TextEdit::Insert { text, .. },
    ] if *atoms == [AtomId::Seed(2)] && text == "\n"));

    let concurrent = TextEdit::FormatParagraph { after: None, change: ParagraphFormatChange::Align(Some(Align::Left)) };
    replica.apply(&mut current, &styles, "bob", 1, &[concurrent]).unwrap();
    replica.apply(&mut current, &styles, "alice", 1, &edits).unwrap();
    assert_eq!(current.text, draft.text);
    assert_eq!(current.paras[1].para.align, Some(Align::Left));
    replica.set_batch_active(&mut current, "alice", 1, false).unwrap();
    assert_eq!(current.paras[1].para.align, Some(Align::Center));
}
