use std::sync::Arc;

use designcraft_collab::{Change, CommentAnchor, Error, Operation, Replica, SharedId};
use designcraft_collab::{
    layers::LayerEdit,
    pages::PageEdit,
    text::{AtomId, BasicTextFormat, MAX_EDIT_ATOMS, TextEdit},
};
use designcraft_doc::build::NewDocument;
use designcraft_doc::color::swatch::BLACK;
use designcraft_doc::geom::{Affine, Rect, shapes};
use designcraft_doc::{
    Content, Document, Fill, Hyperlink, HyperlinkDest, HyperlinkSource, Item, ItemId, LayerId, Shape, SpreadRef, StoryId, TextSel,
};

mod automerge_stories;

fn seed() -> Document {
    Document::new(&NewDocument::default())
}

fn add_frame(doc: &mut Document, x: f64) -> ItemId {
    let id = ItemId(doc.alloc());
    let path = shapes::rectangle(Rect::new(x, 0.0, x + 10.0, 20.0));
    let frame = Item::new(id, doc.default_layer(), Shape::Rectangle, path);
    doc.insert_item(SpreadRef::Doc(0), frame, None).unwrap();
    id
}

fn add_text_frame(doc: &mut Document, x: f64) -> (ItemId, StoryId) {
    doc.add_text_frame(SpreadRef::Doc(0), Rect::new(x, 0.0, x + 20.0, 20.0), doc.default_layer(), "Seed story", Default::default()).unwrap()
}

fn id(actor: &str, sequence: u64) -> SharedId {
    SharedId::Created { actor: actor.into(), sequence, index: 0 }
}

fn normalized(doc: &Document, replica: &Replica) -> Vec<(SharedId, Item)> {
    doc.spreads
        .iter()
        .flat_map(|spread| &spread.items)
        .map(|item| {
            let mut value = item.as_ref().clone();
            value.id = ItemId(0);
            (replica.shared_id(item.id).unwrap(), value)
        })
        .collect()
}

#[test]
fn concurrent_creates_and_independent_properties_converge_across_three_allocators() {
    let base = seed();
    let mut devices = [(base.clone(), Replica::new(&base)), (base.clone(), Replica::new(&base)), (base.clone(), Replica::new(&base))];
    devices[2].0.next_id += 240;

    let mut alice_draft = devices[0].0.clone();
    let mut bob_draft = devices[1].0.clone();
    let alice_numeric = add_frame(&mut alice_draft, 5.0);
    let bob_numeric = add_frame(&mut bob_draft, 200.0);
    assert_eq!(alice_numeric, bob_numeric);

    let a = devices[0].1.capture(&devices[0].0, &alice_draft, "alice", 1).unwrap();
    let b = devices[1].1.capture(&devices[1].0, &bob_draft, "bob", 1).unwrap();
    assert!(!a[0].conflicts(&b[0]));
    for (doc, replica) in &mut devices {
        replica.apply(doc, &b).unwrap();
        replica.apply(doc, &a).unwrap();
        assert_ne!(replica.local_id(&id("alice", 1)), replica.local_id(&id("bob", 1)));
    }
    assert_ne!(devices[0].1.local_id(&id("alice", 1)), devices[2].1.local_id(&id("alice", 1)));

    let alice_id = devices[0].1.local_id(&id("alice", 1)).unwrap();
    let mut geometry_draft = devices[0].0.clone();
    let frame = geometry_draft.item_mut(alice_id).unwrap();
    frame.path.transform(Affine::scale_non_uniform(2.0, 3.0));
    frame.xf = Affine::translate((75.0, 50.0)) * Affine::rotate(0.2);
    let transform = frame.xf;
    let geometry = devices[0].1.capture(&devices[0].0, &geometry_draft, "alice", 2).unwrap();

    let bob_item = devices[1].1.local_id(&id("alice", 1)).unwrap();
    let mut paint_draft = devices[1].0.clone();
    paint_draft.item_mut(bob_item).unwrap().fill = Fill::swatch(BLACK);
    let paint = devices[1].1.capture(&devices[1].0, &paint_draft, "bob", 2).unwrap();
    assert!(!geometry[0].conflicts(&paint[0]));

    let third_item = devices[2].1.local_id(&id("alice", 1)).unwrap();
    let mut stroke_draft = devices[2].0.clone();
    let stroke = &mut stroke_draft.item_mut(third_item).unwrap().stroke;
    stroke.weight = 3.25;
    stroke.tint = 0.4;
    let expected_stroke = stroke.clone();
    let stroke = devices[2].1.capture(&devices[2].0, &stroke_draft, "charlie", 1).unwrap();
    assert!(matches!(stroke.as_slice(), [Operation::Patch { change: Change::Stroke(_), .. }]));
    assert!(!stroke[0].conflicts(&geometry[0]));
    assert!(!stroke[0].conflicts(&paint[0]));

    for (i, (doc, replica)) in devices.iter_mut().enumerate() {
        // Host order matters for create/stack edits. Independent field edits
        // also commute when a relay delivers them in another order.
        let edits = match i {
            0 => [&geometry, &paint, &stroke],
            1 => [&stroke, &paint, &geometry],
            _ => [&paint, &geometry, &stroke],
        };
        for edit in edits {
            replica.apply(doc, edit).unwrap();
        }
        let item = doc.item(replica.local_id(&id("alice", 1)).unwrap()).unwrap();
        assert_eq!(item.fill, Fill::swatch(BLACK));
        assert_eq!(item.stroke, expected_stroke);
        assert_eq!(item.xf, transform);
        assert!(item.inner_bounds().width() > 10.0);
    }
    let expected = normalized(&devices[0].0, &devices[0].1);
    assert_eq!(expected, normalized(&devices[1].0, &devices[1].1));
    assert_eq!(expected, normalized(&devices[2].0, &devices[2].1));
}

#[test]
fn layer_removal_checks_live_references_and_retains_the_last_layer() {
    use designcraft_doc::{Guide, MAX_NEXT_ID, Orientation};

    let mut initial = seed();
    let original = initial.working_layer();
    let mut replica = Replica::new(&initial);
    let remove_base = Operation::Layer { edit: LayerEdit::Remove { layers: vec![original], into: None } };
    let before = initial.clone();
    assert_eq!(replica.apply(&mut initial, &[remove_base]), Err(Error::Invalid("layer removal")));
    assert_eq!(initial, before);

    let mut layer = initial.layers[0].clone();
    layer.name = "Guide layer".into();
    let create = Operation::Layer { edit: LayerEdit::Create { layer, before: Some(original) } };
    initial.next_id = MAX_NEXT_ID;
    let before = initial.clone();
    assert_eq!(replica.apply(&mut initial, std::slice::from_ref(&create)), Err(Error::Limit));
    assert_eq!(initial, before);
    initial.next_id = MAX_NEXT_ID - 1;
    replica.apply(&mut initial, &[create]).unwrap();
    let guide_layer = initial.layers[0].id;
    assert_eq!(guide_layer, LayerId(MAX_NEXT_ID));
    Arc::make_mut(&mut initial.spreads[0]).pages[0].guides.push(Guide {
        orientation: Orientation::Horizontal,
        position: 40.0,
        spread: false,
        locked: false,
        layer: Some(guide_layer),
        liquid: false,
        modular_row: false,
    });
    let with_guide = initial.clone();
    for into in [None, Some(original)] {
        let remove = Operation::Layer { edit: LayerEdit::Remove { layers: vec![guide_layer], into } };
        assert_eq!(replica.apply(&mut initial, &[remove]), Err(Error::UnsupportedEdit));
        assert_eq!(initial, with_guide);
    }
}

#[test]
fn a_mixed_edit_cannot_drop_its_unsupported_changes() {
    let mut initial = seed();
    let local = add_frame(&mut initial, 50.0);
    let mut draft = initial.clone();
    draft.item_mut(local).unwrap().fill = Fill::swatch(BLACK);
    draft.title = "Local-only document change".into();
    let replica = Replica::new(&initial);
    assert_eq!(replica.capture(&initial, &draft, "alice", 1), Err(Error::UnsupportedEdit));
}

#[test]
fn malformed_batch_does_not_leave_a_partial_frame_or_identity() {
    let mut initial = seed();
    let mut draft = initial.clone();
    add_frame(&mut draft, 50.0);
    let mut replica = Replica::new(&initial);
    let mut batch = replica.capture(&initial, &draft, "alice", 1).unwrap();
    let stable = id("alice", 1);
    batch.push(Operation::Patch { id: stable.clone(), change: Change::Layer(LayerId(u64::MAX)) });
    assert_eq!(replica.apply(&mut initial, &batch), Err(Error::UnknownResource));
    assert!(initial.spreads[0].items.is_empty());
    assert_eq!(replica.local_id(&stable), None);

    replica.apply(&mut initial, &batch[..1]).unwrap();
    assert!(replica.local_id(&stable).is_some());
    assert_eq!(replica.apply(&mut initial, &batch[..1]), Err(Error::DuplicateId));
}

#[test]
fn malformed_geometry_and_nonshape_targets_leave_seed_unchanged() {
    let mut initial = seed();
    let shape = add_frame(&mut initial, 100.0);
    let group = add_frame(&mut initial, 50.0);
    let unsupported = initial.item_mut(group).unwrap();
    unsupported.shape = Shape::Group;
    unsupported.content = Content::Group { items: Vec::new() };
    let layer = initial.default_layer();
    let (text, _) = initial.add_text_frame(SpreadRef::Doc(0), Rect::new(0.0, 0.0, 20.0, 20.0), layer, "Seed story", Default::default()).unwrap();
    let mut draft = initial.clone();
    draft.item_mut(shape).unwrap().xf = Affine::translate((50.0, 0.0));
    let mut replica = Replica::new(&initial);
    let mut edits = replica.capture(&initial, &draft, "alice", 1).unwrap();
    let Operation::Patch { id, change: Change::Geometry(_) } = &mut edits[0] else { panic!("expected a geometry patch") };
    *id = SharedId::Seed(group);
    let expected = initial.clone();
    assert_eq!(replica.apply(&mut initial, &edits), Err(Error::UnsupportedObject));
    assert_eq!(initial, expected);
    let Operation::Patch { id, change: Change::Geometry(geometry) } = &mut edits[0] else { panic!("expected a geometry patch") };
    *id = SharedId::Seed(shape);
    geometry.transform[0] = f64::NAN;
    assert_eq!(replica.apply(&mut initial, &edits), Err(Error::Invalid("transform")));
    assert_eq!(initial, expected);
    let remove_graphic = Operation::SetGraphic { id: SharedId::Seed(text), placement: None };
    assert_eq!(replica.apply(&mut initial, &[remove_graphic]), Err(Error::UnsupportedObject));
    assert_eq!(initial, expected);
}

#[test]
fn reordered_objects_keep_their_layer_and_delete_conflicts_with_stale_edits() {
    let mut initial = seed();
    let first = add_frame(&mut initial, 50.0);
    add_frame(&mut initial, 100.0);
    let mut alternate = initial.layers[0].clone();
    alternate.id = LayerId(initial.alloc());
    let layer = alternate.id;
    initial.layers.push(alternate);
    let mut draft = initial.clone();
    draft.item_mut(first).unwrap().layer = layer;
    Arc::make_mut(&mut draft.spreads[0]).items.reverse();
    let mut replica = Replica::new(&initial);
    let order = replica.capture(&initial, &draft, "alice", 1).unwrap();
    assert!(order.iter().any(|op| matches!(op, Operation::Reorder { .. })));
    let delete = Operation::Delete { id: SharedId::Seed(first), spread: initial.spreads[0].id };
    assert!(order.iter().all(|op| op.conflicts(&delete)));
    replica.apply(&mut initial, &order).unwrap();
    assert_eq!(initial, draft);
}

#[test]
fn a_page_removal_cannot_bypass_frame_reference_checks() {
    let mut before = Document::new(&NewDocument { pages: 3, facing_pages: true, ..Default::default() });
    let frame = add_frame(&mut before, 25.0);
    let hyperlink = before.alloc();
    before.hyperlinks.push(Hyperlink {
        id: hyperlink,
        name: "Reference".into(),
        source: HyperlinkSource::Item { id: frame },
        dest: HyperlinkDest::Url("https://example.com".into()),
    });
    let shared = SharedId::Seed(frame);
    let mut replica = Replica::new(&before);
    let mut draft = before.clone();
    draft.delete_pages(&[0]).unwrap();
    assert!(draft.item(frame).is_none());
    assert_eq!(replica.capture(&before, &draft, "alice", 1), Err(Error::UnsupportedObject));

    let mut confirmed = before.clone();
    let edit = PageEdit::Remove { pages: vec![before.page(0).unwrap().id], allocator: before.next_id };
    assert_eq!(replica.apply(&mut confirmed, &[Operation::Page { edit }]), Err(Error::UnsupportedObject));
    assert_eq!(confirmed, before);
    assert_eq!(replica.local_id(&shared), Some(frame));
}

#[test]
fn deleting_around_a_text_selection_preserves_the_selected_atoms() {
    use std::ops::Range;

    let check = |text: &str, (anchor, focus): (usize, usize), before: Range<usize>, after: Range<usize>, deleted: &[u32]| {
        let mut seed = seed();
        let layer = seed.default_layer();
        let (_, story) = seed.add_text_frame(SpreadRef::Doc(0), Rect::new(0.0, 0.0, 100.0, 20.0), layer, text, Default::default()).unwrap();
        let old = TextSel { anchor, focus, ..TextSel::caret(story, 0) };
        let new = TextSel { anchor: anchor - before.len(), focus: focus - before.len(), ..old };
        let mut draft = seed.clone();
        draft.story_mut(story).unwrap().delete(after);
        draft.story_mut(story).unwrap().delete(before);

        let mut replica = Replica::new(&seed);
        let operations = replica.capture_selected(&seed, &draft, "alice", 1, Some((old, new))).unwrap();
        let [Operation::Text { edits, .. }] = operations.as_slice() else { panic!("expected text edits, got {operations:?}") };
        let atoms: Vec<_> = edits
            .iter()
            .flat_map(|edit| match edit {
                TextEdit::Delete { atoms } => atoms.iter().cloned(),
                _ => panic!("expected only deletions, got {edit:?}"),
            })
            .collect();
        assert_eq!(atoms, deleted.iter().copied().map(AtomId::Seed).collect::<Vec<_>>());

        replica.apply(&mut seed, &operations).unwrap();
        let expected = draft.story(story).unwrap();
        let replayed = seed.story_mut(story).unwrap();
        replayed.rev = expected.rev;
        assert_eq!(&*replayed, expected);
    };

    // The same text results if the second newline is removed instead, but its atom is selected.
    check("A\n\nB", (2, 3), 1..2, 3..3, &[1]);
    // Delete on both sides of a reverse selection with a multibyte character and a newline.
    check("Pα\nλ\n\nαZ", (7, 4), 1..4, 7..10, &[1, 2, 5, 6]);
}

#[test]
fn text_comments_follow_original_utf8_atoms_across_edits_undo_and_replay() {
    let mut seed = seed();
    let layer = seed.default_layer();
    let (frame, story) = seed.add_text_frame(SpreadRef::Doc(0), Rect::new(0.0, 0.0, 20.0, 20.0), layer, "Aé😊Z", Default::default()).unwrap();
    let mut current = seed.clone();
    let mut replica = Replica::new(&seed);
    // The recorded legacy anchor initializes the old text model even when new room builds
    // prefer Automerge for unedited stories.
    let anchor = CommentAnchor::Text { story, atoms: vec![AtomId::Seed(1), AtomId::Seed(2)] };
    let comment = Operation::CreateComment { id: id("alice", 1), anchor: anchor.clone(), text: "Review this range".into() };
    let mut history = vec![comment.clone()];
    replica.apply(&mut current, &[comment]).unwrap();
    assert_eq!(replica.comment_text_range(&current, &anchor), Some((story, 1..7)));

    let insert = |after, text: &str| TextEdit::Insert { after: Some(AtomId::Seed(after)), text: text.into(), format: BasicTextFormat::default() };
    let insertion = Operation::Text { story, actor: "bob".into(), sequence: 1, edits: vec![insert(0, "!"), insert(1, "文"), insert(2, "?")] };
    replica.apply(&mut current, std::slice::from_ref(&insertion)).unwrap();
    history.push(insertion);
    let range = replica.comment_text_range(&current, &anchor).unwrap().1;
    assert_eq!(&current.story(story).unwrap().text[range], "é文😊");

    // A freshly captured range containing an insertion must follow story order, even when its IDs sort differently.
    let edited_anchor = replica.text_comment_anchor(&current, story, 2..11).unwrap();
    let edited_comment = Operation::CreateComment { id: id("alice", 2), anchor: edited_anchor, text: "Includes the insertion".into() };
    replica.apply(&mut current, std::slice::from_ref(&edited_comment)).unwrap();
    history.push(edited_comment);

    let delete =
        |sequence, seed| Operation::Text { story, actor: "bob".into(), sequence, edits: vec![TextEdit::Delete { atoms: vec![AtomId::Seed(seed)] }] };
    let first_deletion = delete(2, 1);
    replica.apply(&mut current, std::slice::from_ref(&first_deletion)).unwrap();
    history.push(first_deletion);
    let range = replica.comment_text_range(&current, &anchor).unwrap().1;
    assert_eq!(&current.story(story).unwrap().text[range], "😊");

    let second_deletion = delete(3, 2);
    let late_comment = Operation::CreateComment { id: id("alice", 3), anchor: anchor.clone(), text: "Sent before the deletions".into() };
    for operation in [second_deletion, late_comment] {
        replica.apply(&mut current, std::slice::from_ref(&operation)).unwrap();
        history.push(operation);
    }
    assert_eq!(replica.comment_text_range(&current, &anchor), None);
    assert!(replica.comments().filter(|thread| thread.anchor == anchor).all(|thread| replica.comment_is_orphaned(&current, thread)));

    let undo = Operation::SetTextBatchActive { story, actor: "bob".into(), original_sequence: 2, active: false };
    replica.apply(&mut current, std::slice::from_ref(&undo)).unwrap();
    history.push(undo);
    let range = replica.comment_text_range(&current, &anchor).unwrap().1;
    assert_eq!(&current.story(story).unwrap().text[range], "é");
    assert!(replica.comments().all(|thread| !replica.comment_is_orphaned(&current, thread)));

    let mut replay = Replica::new(&seed);
    let mut reopened = seed;
    let wire = serde_json::to_vec(&history).unwrap();
    for operation in serde_json::from_slice::<Vec<Operation>>(&wire).unwrap() {
        replay.apply(&mut reopened, &[operation]).unwrap();
    }
    assert_eq!(reopened, current);
    assert!(replay.comments().eq(replica.comments()));
    assert_eq!(replay.comment_text_range(&reopened, &anchor), replica.comment_text_range(&current, &anchor));

    let removal = Operation::DeleteText { id: SharedId::Seed(frame), spread: reopened.spreads[0].id, story };
    replay.apply(&mut reopened, &[removal]).unwrap();
    assert!(replay.comments().all(|thread| replay.comment_is_orphaned(&reopened, thread)));
    let stale = Operation::CreateComment { id: id("alice", 4), anchor, text: "The entire story has gone".into() };
    assert_eq!(replay.apply(&mut reopened, &[stale]), Err(Error::UnknownResource));
}

#[test]
fn text_comments_reject_invalid_ranges_before_creating_a_thread() {
    let mut document = seed();
    let layer = document.default_layer();
    let text = "é".repeat(MAX_EDIT_ATOMS + 1);
    let (_, story) = document.add_text_frame(SpreadRef::Doc(0), Rect::new(0.0, 0.0, 20.0, 20.0), layer, &text, Default::default()).unwrap();
    let (_, other_story) = add_text_frame(&mut document, 30.0);
    let mut replica = Replica::new(&document);
    assert_eq!(replica.text_comment_anchor(&document, story, 0..0), None);
    assert_eq!(replica.text_comment_anchor(&document, story, 1..2), None);
    assert_eq!(replica.text_comment_anchor(&document, story, 0..text.len()), None);
    let limit_anchor = replica.text_comment_anchor(&document, story, 0..MAX_EDIT_ATOMS * 'é'.len_utf8()).unwrap();
    let foreign = AtomId::Inserted { actor: "bob".into(), sequence: 1, index: 0 };
    let edit = Operation::Text {
        story: other_story,
        actor: "bob".into(),
        sequence: 1,
        edits: vec![TextEdit::Insert { after: None, text: "!".into(), format: BasicTextFormat::default() }],
    };
    replica.apply(&mut document, &[edit]).unwrap();

    let invalid_ranges = [
        (vec![], Error::Invalid("text comment range")),
        (vec![AtomId::Seed(0), AtomId::Seed(0)], Error::Invalid("text comment range")),
        (vec![AtomId::Seed(1), AtomId::Seed(0)], Error::Invalid("text comment range")),
        (vec![foreign], Error::Invalid("text comment range")),
        ((0..=MAX_EDIT_ATOMS as u32).map(AtomId::Seed).collect(), Error::Limit),
    ];
    let make_comment = |anchor| Operation::CreateComment { id: id("alice", 1), anchor, text: "A selection comment".into() };
    for (atoms, error) in invalid_ranges {
        let operation = make_comment(CommentAnchor::Text { story, atoms });
        assert_eq!(replica.apply(&mut document, &[operation]), Err(error));
        assert_eq!(replica.comments().count(), 0);
    }
    replica.apply(&mut document, &[make_comment(limit_anchor)]).unwrap();
    assert_eq!(replica.comments().count(), 1);
}

#[test]
fn deleting_a_simple_text_frame_replays_native_removal_and_preserves_its_comments() {
    let mut seed = seed();
    let (frame, story) = add_text_frame(&mut seed, 0.0);
    let (survivor, surviving_story) = add_text_frame(&mut seed, 50.0);
    let shared = SharedId::Seed(frame);
    let thread = id("alice", 1);
    let mut history = vec![
        Operation::CreateComment { id: thread.clone(), anchor: CommentAnchor::Object(shared.clone()), text: "Review the copy".into() },
        Operation::Text {
            story,
            actor: "bob".into(),
            sequence: 1,
            edits: vec![TextEdit::Insert { after: None, text: "Edited: ".into(), format: BasicTextFormat::default() }],
        },
    ];
    let mut replica = Replica::new(&seed);
    let mut current = seed.clone();
    for operation in &history {
        replica.apply(&mut current, std::slice::from_ref(operation)).unwrap();
    }
    let comment = replica.comments().next().unwrap();
    assert!(!replica.comment_is_orphaned(&current, comment));
    assert!(replica.text_byte_after(story, Some(&AtomId::Seed(0))).is_some());

    let mut native = current.clone();
    native.remove_item(frame).unwrap();
    let deletion = replica.capture(&current, &native, "alice", 2).unwrap();
    assert_eq!(replica.inverse_patch_batch(&current, &deletion), Ok(None));
    replica.apply(&mut current, &deletion).unwrap();
    history.extend(deletion);
    assert_eq!(current, native);
    assert!(current.story(story).is_none());
    assert_eq!(current.item(survivor), seed.item(survivor));
    assert_eq!(current.story(surviving_story), seed.story(surviving_story));
    assert_eq!(current.next_id, seed.next_id);
    assert_eq!(replica.shared_id(frame), None);
    assert_eq!(replica.local_id(&shared), None);
    assert_eq!(replica.text_byte_after(story, Some(&AtomId::Seed(0))), None);

    let followup = vec![
        Operation::Reply { thread: thread.clone(), id: id("bob", 2), text: "Reviewed before removal".into() },
        Operation::SetResolved { thread, resolved: true },
        Operation::CreateComment { id: id("alice", 3), anchor: CommentAnchor::Object(shared), text: "Sent before I saw the removal".into() },
    ];
    for operation in &followup {
        replica.apply(&mut current, std::slice::from_ref(operation)).unwrap();
    }
    history.extend(followup);
    let mut replay = Replica::new(&seed);
    let mut reopened = seed;
    let wire = serde_json::to_vec(&history).unwrap();
    for operation in serde_json::from_slice::<Vec<Operation>>(&wire).unwrap() {
        replay.apply(&mut reopened, &[operation]).unwrap();
    }
    assert_eq!(reopened, current);
    assert!(replay.comments().eq(replica.comments()));
    assert!(replay.comments().all(|thread| replay.comment_is_orphaned(&reopened, thread)));
    let thread = replay.comments().next().unwrap();
    assert_eq!(thread.replies.len(), 1);
    assert!(thread.resolved);
}

#[test]
fn text_frame_deletion_conflicts_with_its_story_and_requires_a_standalone_batch() {
    let mut document = seed();
    let (frame, story) = add_text_frame(&mut document, 0.0);
    let (other, other_story) = add_text_frame(&mut document, 50.0);
    let id = SharedId::Seed(frame);
    let spread = document.spreads[0].id;
    let delete = Operation::DeleteText { id: id.clone(), spread, story };
    let insert = |story| Operation::Text {
        story,
        actor: "bob".into(),
        sequence: 1,
        edits: vec![TextEdit::Insert { after: None, text: "New text ".into(), format: BasicTextFormat::default() }],
    };
    let edit = insert(story);
    let edit_other = insert(other_story);
    assert!(delete.conflicts(&edit) && edit.conflicts(&delete));
    assert!(!delete.conflicts(&edit_other) && !edit_other.conflicts(&delete));
    assert!(delete.conflicts(&Operation::Patch { id: id.clone(), change: Change::Fill(Fill::swatch(BLACK)) }));
    assert!(delete.conflicts(&Operation::Reorder { spread, items: vec![SharedId::Seed(other), id.clone()] }));

    let expected = document.clone();
    let mut replica = Replica::new(&document);
    assert_eq!(replica.apply(&mut document, &[edit.clone(), delete.clone()]), Err(Error::UnsupportedEdit));
    assert_eq!(replica.apply(&mut document, &[Operation::DeleteText { id: id.clone(), spread, story: other_story }]), Err(Error::UnsupportedObject));
    assert_eq!(replica.apply(&mut document, &[Operation::Delete { id, spread }]), Err(Error::UnsupportedObject));
    assert_eq!(document, expected);

    let mut mixed_draft = expected.clone();
    mixed_draft.remove_item(frame).unwrap();
    mixed_draft.title = "Unrelated change".into();
    assert_eq!(replica.capture(&expected, &mixed_draft, "alice", 1), Err(Error::UnsupportedEdit));
    mixed_draft.title.clone_from(&expected.title);
    mixed_draft.alloc();
    assert_eq!(replica.capture(&expected, &mixed_draft, "alice", 1), Err(Error::UnsupportedEdit));

    replica.apply(&mut document, &[delete]).unwrap();
    assert_eq!(replica.apply(&mut document, &[edit]), Err(Error::UnknownResource));
    replica.apply(&mut document, &[edit_other]).unwrap();
    assert_eq!(document.story(other_story).unwrap().text, "New text Seed story");
}

#[test]
fn shared_deletion_rejects_threaded_and_referenced_stories() {
    let mut document = seed();
    let (frame, story) = add_text_frame(&mut document, 0.0);
    let (other, _) = add_text_frame(&mut document, 50.0);
    let mut threaded = document.clone();
    threaded.thread(frame, other).unwrap();
    let mut primary = document.clone();
    primary.settings.primary_story = Some(story);
    let mut linked = document.clone();
    linked.hyperlinks.push(Hyperlink {
        id: 1,
        name: "Website".into(),
        source: HyperlinkSource::Text { story, start: 0, end: 4 },
        dest: HyperlinkDest::Url("https://example.com".into()),
    });
    let mut advanced = document;
    advanced.story_mut(story).unwrap().chars[0].format.over.baseline_shift = Some(2.0);

    for mut before in [threaded, primary, linked, advanced] {
        let expected = before.clone();
        let mut native = before.clone();
        native.remove_item(frame).unwrap();
        let mut replica = Replica::new(&before);
        let deletion = Operation::DeleteText { id: SharedId::Seed(frame), spread: before.spreads[0].id, story };
        assert!(replica.capture(&before, &native, "alice", 1).is_err());
        assert!(replica.apply(&mut before, &[deletion]).is_err());
        assert_eq!(before, expected);
        assert_eq!(replica.shared_id(frame), Some(SharedId::Seed(frame)));
    }
}

#[test]
fn compensating_batches_restore_only_domains_touched_by_the_original_edit() {
    let mut document = seed();
    let frame = add_frame(&mut document, 50.0);
    add_frame(&mut document, 100.0);
    let mut replica = Replica::new(&document);
    let thread = id("alice", 1);
    replica
        .apply(
            &mut document,
            &[Operation::CreateComment { id: thread.clone(), anchor: CommentAnchor::Object(SharedId::Seed(frame)), text: "Review".into() }],
        )
        .unwrap();

    let mut alice_draft = document.clone();
    alice_draft.item_mut(frame).unwrap().xf = Affine::translate((75.0, 25.0));
    Arc::make_mut(&mut alice_draft.spreads[0]).items.reverse();
    let mut alice = replica.capture(&document, &alice_draft, "alice", 2).unwrap();
    alice.push(Operation::SetResolved { thread: thread.clone(), resolved: true });
    let undo = replica.inverse_patch_batch(&document, &alice).unwrap().unwrap();
    let mut expected_undone = document.clone();
    replica.apply(&mut document, &alice).unwrap();

    let bob = [
        Operation::Patch { id: SharedId::Seed(frame), change: Change::Fill(Fill::swatch(BLACK)) },
        Operation::Reply { thread, id: id("bob", 1), text: "New paint".into() },
    ];
    assert!(undo.iter().all(|edit| bob.iter().all(|later| !edit.conflicts(later))));
    replica.apply(&mut document, &bob).unwrap();
    expected_undone.item_mut(frame).unwrap().fill = Fill::swatch(BLACK);
    let expected_redone = document.clone();

    let redo = replica.inverse_patch_batch(&document, &undo).unwrap().unwrap();
    replica.apply(&mut document, &undo).unwrap();
    assert_eq!(document, expected_undone);
    assert!(!replica.comments().next().unwrap().resolved);
    assert_eq!(replica.comments().next().unwrap().replies[0].text, "New paint");

    replica.apply(&mut document, &redo).unwrap();
    assert_eq!(document, expected_redone);
    assert!(replica.comments().next().unwrap().resolved);
    assert_eq!(replica.comments().next().unwrap().replies[0].text, "New paint");
}

#[test]
fn a_mixed_structural_batch_has_no_compensation() {
    let document = seed();
    let mut draft = document.clone();
    add_frame(&mut draft, 50.0);
    let replica = Replica::new(&document);
    let mut edits = replica.capture(&document, &draft, "alice", 1).unwrap();
    edits.push(Operation::Patch { id: id("alice", 1), change: Change::Fill(Fill::swatch(BLACK)) });
    assert_eq!(replica.inverse_patch_batch(&document, &edits), Ok(None));
}

#[test]
fn replies_and_resolution_survive_deleting_their_anchor_and_replaying_the_log() {
    let mut initial = seed();
    let local = add_frame(&mut initial, 50.0);
    let item = SharedId::Seed(local);
    let thread = id("alice", 1);
    let operations = vec![
        Operation::CreateComment { id: thread.clone(), anchor: CommentAnchor::Object(item.clone()), text: "Adjust this frame".into() },
        Operation::Reply { thread: thread.clone(), id: id("bob", 1), text: "Ready to review".into() },
        Operation::Delete { id: item, spread: initial.spreads[0].id },
        Operation::SetResolved { thread, resolved: true },
    ];
    let encoded = serde_json::to_vec(&operations).unwrap();
    let replay: Vec<Operation> = serde_json::from_slice(&encoded).unwrap();
    let mut first_doc = initial.clone();
    let mut first_replica = Replica::new(&initial);
    first_replica.apply(&mut first_doc, &operations[..2]).unwrap();
    assert!(!first_replica.comment_is_orphaned(&first_doc, first_replica.comments().next().unwrap()));
    first_replica.apply(&mut first_doc, &operations[2..]).unwrap();

    let mut reopened_doc = initial.clone();
    let mut reopened_replica = Replica::new(&initial);
    reopened_replica.apply(&mut reopened_doc, &replay).unwrap();
    let reopened_thread = reopened_replica.comments().next().unwrap();
    assert!(reopened_thread.resolved);
    assert!(reopened_replica.comment_is_orphaned(&reopened_doc, reopened_thread));
    assert_eq!(reopened_thread.id.author(), Some("alice"));
    assert_eq!(reopened_thread.replies[0].id.author(), Some("bob"));
    assert_eq!(Some(reopened_thread), first_replica.comments().next());
    assert_eq!(first_doc, reopened_doc);
}
