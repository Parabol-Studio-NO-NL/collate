use std::sync::Arc;

use designcraft_doc::{
    Asset, AssetData, AssetId, Content, Document, Graphic, Item, ItemId, ParaFormat, Shape, SpreadRef, StoryId, build::NewDocument,
};
use designcraft_engine::Session;
use designcraft_geom::{Affine, Rect, shapes};
use serde_json::json;

use super::{artwork_fixture_with_side, document, editor, room, wait_for, wait_for_revision};
use crate::{
    DesignApp, Services,
    collaboration::{Room, replay, seed_document::fingerprint},
};

const PAGES: usize = 200;
const PAGES_PER_CHAPTER: usize = 10;
const ILLUSTRATIONS: u32 = 16;
const SIDE: u32 = 192;

// Original sample text and illustrations, generated for this Collate test.
fn book() -> (Document, StoryId, ItemId) {
    let mut doc = Document::new(&NewDocument { title: "The River Survey".into(), pages: PAGES, ..Default::default() });
    let assets: Vec<AssetId> = (1..=ILLUSTRATIONS)
        .map(|number| {
            let id = AssetId(doc.alloc());
            let asset = Asset {
                id,
                name: format!("Survey illustration {number}.png"),
                mime: "image/png".into(),
                data: AssetData::embedded(artwork_fixture_with_side(number, SIDE)),
                pixels: Some((SIDE, SIDE)),
                ..Default::default()
            };
            doc.assets.insert(id, Arc::new(asset));
            id
        })
        .collect();
    let layer = doc.default_layer();
    let para = ParaFormat { style: doc.styles.default_paragraph.clone(), ..Default::default() };
    let mut chapter = None;
    let mut previous_frame = None;
    let mut illustration = None;
    for index in 0..PAGES {
        let (spread, _) = doc.page_loc(index).unwrap();
        let page_x = doc.page(index).unwrap().x;
        let destination = SpreadRef::Doc(spread);
        let start = index.is_multiple_of(PAGES_PER_CHAPTER);
        let text = if start { chapter_text(index / PAGES_PER_CHAPTER + 1) } else { String::new() };
        let (frame, story) =
            doc.add_text_frame(destination, Rect::new(page_x + 54.0, 54.0, page_x + 558.0, 588.0), layer, &text, para.clone()).unwrap();
        if start {
            chapter = Some(story);
        } else {
            doc.thread(previous_frame.unwrap(), frame).unwrap();
        }
        previous_frame = Some(frame);

        let id = ItemId(doc.alloc());
        let mut item = Item::new(id, layer, Shape::Rectangle, shapes::rectangle(Rect::new(0.0, 0.0, 144.0, 144.0)));
        item.xf = Affine::translate((page_x + 54.0, 604.0));
        item.content = Content::Graphic(Graphic {
            asset: assets[index % assets.len()],
            size: (f64::from(SIDE), f64::from(SIDE)),
            xf: Affine::scale(144.0 / f64::from(SIDE)),
            layer_options: None,
            clip: None,
            auto_fit: Default::default(),
            fit_align: 4,
            crop: [0.0; 4],
            wrap: Default::default(),
            fill: Default::default(),
        });
        doc.insert_item(destination, item, None).unwrap();
        illustration = Some(id);
    }
    (doc, chapter.unwrap(), illustration.unwrap())
}

fn chapter_text(chapter: usize) -> String {
    let observation = concat!(
        "At dawn the survey crew returned to the river. They found a line of mica in the clay, ",
        "and measured the distance from the last stake to the footbridge. The route along the bank ",
        "curved beneath an alder. In the field book they drew the bend, then marked a second route ",
        "above the water where the broken steps began. The afternoon notes compared the two routes ",
        "and recorded where the river had cut into the path after the spring rains. By evening they ",
        "had enough detail to redraw the map and check it against the previous survey.\n"
    );
    let mut text = format!("Chapter {chapter}\n");
    text.push_str(&observation.repeat(PAGES_PER_CHAPTER * 2));
    text
}

fn receive_invitation(invitation: &str, name: &str) -> DesignApp {
    let mut app = DesignApp::new(Session::new(), Services::default());
    app.collaboration.name = name.into();
    app.collaboration.join_code = invitation.into();
    app.start_seed_download().unwrap();
    app
}

#[test]
fn illustrated_book_converges_through_concurrent_editing_late_join_and_recovery() {
    let (seed, chapter, illustration) = book();
    let last_chapter = seed.story(chapter).unwrap();
    assert_eq!(last_chapter.frames.len(), PAGES_PER_CHAPTER);

    let directory = tempfile::tempdir().unwrap();
    let snapshot = directory.path().join("Survey.designcraft");
    let mut host = editor(&seed, "Book designer");
    host.run("file.saveACopy", json!({"path": snapshot})).unwrap();
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "book room starts", |apps| room(apps[0]).can_edit() && room(apps[0]).invitation.is_some());
    assert_eq!(room(&host).storage.seed_blobs.len(), seed.assets.len());
    let invitation = room(&host).invitation.clone().unwrap();
    let mut guest = receive_invitation(&invitation, "Remote editor");
    wait_for(&mut [&mut host, &mut guest], "first guest receives the illustrated book", |apps| {
        apps[1].collaboration.room.as_ref().is_some_and(Room::can_edit)
    });
    assert_eq!(document(&guest), &seed);

    // Both peers form their edits against the original version of the same threaded story.
    host.run("text.select", json!({"story": chapter.0, "anchor": 0})).unwrap();
    host.run("text.insert", json!({"text": "Révision\n", "raw": true})).unwrap();
    guest.run("text.select", json!({"story": chapter.0, "anchor": last_chapter.text.len()})).unwrap();
    guest.run("text.insert", json!({"text": "Café by the bridge.", "raw": true})).unwrap();
    let queued = if cfg!(feature = "automerge-rooms") {
        guest.poll_collaboration(&egui::Context::default());
        assert!(room(&guest).pending.is_some());
        let queued = " Notes from the bank.";
        assert!(guest.buffer_shared_typing(queued));
        queued
    } else {
        ""
    };
    let mut revision = if queued.is_empty() { 2 } else { 3 };
    wait_for_revision(&mut host, &mut guest, "both chapter edits and any buffered typing arrive", revision);
    let text = format!("Révision\n{}Café by the bridge.{queued}", last_chapter.text);
    assert_eq!(document(&host).story(chapter).unwrap().text, text);
    if cfg!(feature = "automerge-rooms") {
        assert!(room(&host).history.iter().all(|(commit, _)| commit.proposal.story_transaction.is_some()));
        let pasted = chapter_text(PAGES / PAGES_PER_CHAPTER).repeat(6);
        guest.run("text.select", json!({"story": chapter.0, "anchor": text.len()})).unwrap();
        guest.run("edit.paste", json!({"text": pasted})).unwrap();
        revision += 1;
        wait_for_revision(&mut host, &mut guest, "a large clipboard paste arrives as one Automerge transaction", revision);
        assert_eq!(document(&host).story(chapter).unwrap().text, format!("{text}{pasted}"));
        assert!(room(&host).history.last().unwrap().1.len() > designcraft_collab_core::MAX_PAYLOAD);
    }

    let before = seed.item(illustration).unwrap().bounds();
    guest.run("transform.move", json!({"ids": [illustration.0], "dx": 12})).unwrap();
    revision += 1;
    wait_for_revision(&mut host, &mut guest, "illustration moves on the last page", revision);
    assert!(document(&host).item(illustration).unwrap().bounds().x0 > before.x0);
    assert_eq!(document(&guest), document(&host));

    let mut late = receive_invitation(&invitation, "Late proofreader");
    wait_for(&mut [&mut host, &mut guest, &mut late], "late guest receives artwork and chapter edits", |apps| {
        apps[2].collaboration.room.as_ref().is_some_and(|room| room.can_edit() && room.revision == revision)
    });
    assert_eq!(document(&late), document(&host));
    assert_eq!(document(&late).page_count(), PAGES);

    let mut reopened = DesignApp::new(Session::new(), Services::default());
    reopened.run("file.open", json!({"path": snapshot})).unwrap();
    let confirmed = room(&host).history.iter().map(|(commit, _)| commit.clone()).collect();
    let restored = replay::restore(&seed, document(&reopened), confirmed).unwrap();
    assert_eq!(fingerprint(&restored.document).unwrap(), fingerprint(document(&late)).unwrap());
}

#[test]
fn automerge_merges_a_long_chapter_without_replacing_the_rest_of_the_book() {
    use designcraft_collab::{
        automerge_stories::{AutomergeStories, StoryTransaction},
        text::MAX_STORY_ATOMS,
    };

    let (mut seed, chapter, _) = book();
    let long_chapter = seed.story_mut(chapter).unwrap();
    long_chapter.insert(long_chapter.text.len(), &chapter_text(PAGES / PAGES_PER_CHAPTER));
    let original = seed.story(chapter).unwrap();
    let end = original.text.len();
    assert!(original.text.chars().count() > MAX_STORY_ATOMS);

    let independently_edited = |author: &[u8; 32], at, insert| {
        let mut draft = seed.clone();
        draft.story_mut(chapter).unwrap().insert(at, insert);
        let transaction = AutomergeStories::default().capture(&seed, &draft, author, None).unwrap().unwrap();
        let encoded = serde_json::to_vec(&transaction).unwrap();
        assert!(encoded.len() < end, "the edit travels without the chapter or its images");
        serde_json::from_slice::<StoryTransaction>(&encoded).unwrap()
    };
    let editor = &[41; 32];
    let proofreader = &[87; 32];
    let heading = "Révision\n";
    let note = "Café by the bridge.";
    let a = independently_edited(editor, 0, heading);
    let b = independently_edited(proofreader, end, note);

    let replay = |ordered: [(&[u8; 32], &StoryTransaction); 2]| {
        let mut replica = AutomergeStories::default();
        let mut merged = seed.clone();
        for (actor, transaction) in ordered {
            replica.apply(&mut merged, actor, transaction).unwrap();
        }
        merged
    };
    let first = replay([(editor, &a), (proofreader, &b)]);
    let second = replay([(proofreader, &b), (editor, &a)]);
    assert_eq!(first, second);
    let result = first.story(chapter).unwrap();
    assert_eq!(result.text, format!("{heading}{}{note}", original.text));
    assert_eq!(result.frames, original.frames);
    assert!(seed.stories.iter().filter(|(id, _)| **id != chapter).all(|(id, original)| Arc::ptr_eq(original, &first.stories[id])));
    assert!(seed.assets.iter().all(|(id, original)| Arc::ptr_eq(original, &first.assets[id])));
    assert!(seed.spreads.iter().zip(&first.spreads).all(|(original, merged)| Arc::ptr_eq(original, merged)));
}
