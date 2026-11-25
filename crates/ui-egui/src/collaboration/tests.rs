use std::{thread, time::Duration};

use designcraft_collab::{CommentAnchor, Operation, SharedId};
use designcraft_doc::{Align, Document, ItemId, LayerId, RagPattern, StoryId, build::NewDocument};
use designcraft_engine::{DocState, Session};
use serde_json::json;
use web_time::Instant;

use super::Room;
use super::seed_document::{embedded_image_fixture, fingerprint};
use crate::{DesignApp, Services};

fn editor(seed: &Document, name: &str) -> DesignApp {
    let mut session = Session::new();
    session.add_document(DocState::new(seed.clone(), None));
    let mut app = DesignApp::new(session, Services::default());
    app.collaboration.name = name.into();
    app
}

fn room(app: &DesignApp) -> &Room {
    app.collaboration.room.as_ref().unwrap()
}

fn wait_for(apps: &mut [&mut DesignApp], stage: &str, done: impl Fn(&[&mut DesignApp]) -> bool) {
    wait_for_with_deadline(apps, stage, Duration::from_secs(30), done);
}

fn wait_for_with_deadline(apps: &mut [&mut DesignApp], stage: &str, timeout: Duration, done: impl Fn(&[&mut DesignApp]) -> bool) {
    let ctx = egui::Context::default();
    let deadline = Instant::now() + timeout;
    loop {
        for app in apps.iter_mut() {
            app.poll_collaboration(&ctx);
            if let Some(state) = app.collaboration.room.as_ref() {
                assert!(!state.fatal, "{stage}: {}", state.error.as_deref().unwrap_or("collaboration stopped"));
            } else if let Some(error) = app.collaboration.message.as_deref() {
                panic!("{stage}: {error}");
            }
        }
        if done(apps) {
            return;
        }
        if Instant::now() >= deadline {
            let states: Vec<_> = apps
                .iter()
                .map(|app| {
                    app.collaboration
                        .room
                        .as_ref()
                        .map(|state| (state.revision, state.online, state.can_edit(), state.pending.is_some(), state.error.as_deref()))
                })
                .collect();
            panic!("{stage} timed out (revision, online, editable, pending, error): {states:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn join(app: &mut DesignApp, code: &str) {
    app.collaboration.join_code = code.into();
    app.connect_collaboration(false).unwrap();
}

fn document(app: &DesignApp) -> &Document {
    &app.session.doc().unwrap().doc
}

fn deliver_room_packet(sender: &DesignApp, recipient: &mut DesignApp, message: super::Message) {
    let actor = room(sender).local.as_ref().unwrap().actor;
    let bytes = super::encode(message).unwrap();
    recipient.collaboration.room.as_mut().unwrap().on_event(&mut recipient.session, super::Event::Message { actor, bytes });
}

fn assert_converged_layout(doc: &Document, seed: &Document, shape: ItemId, text_frame: ItemId, story: StoryId) {
    use designcraft_doc::{Fill, color::swatch::BLACK};

    assert_eq!(doc.page_count(), seed.page_count());
    assert_eq!(doc.spreads[0].items.iter().map(|item| item.id).collect::<Vec<_>>(), [shape, text_frame]);
    let original = seed.item(shape).unwrap();
    let moved = doc.item(shape).unwrap();
    let bounds = moved.bounds();
    let original_bounds = original.bounds();
    assert!(bounds.x0 > original_bounds.x0 && bounds.y0 > original_bounds.y0);
    assert_eq!(bounds.size(), original_bounds.size());
    assert_ne!(moved.fill, original.fill);
    assert_eq!(moved.fill, Fill::swatch(BLACK));
    let story = doc.story(story).unwrap();
    assert_eq!(story.frames, [text_frame]);
    assert_eq!(story.text, "ABλCD");
}

fn artwork_fixture(noise: u32) -> Vec<u8> {
    artwork_fixture_with_side(noise, 128)
}

fn artwork_fixture_with_side(mut noise: u32, side: u32) -> Vec<u8> {
    use craft_image_codecs::{ChannelLayout, Format, Image};

    let pixels = (0..side * side * 4)
        .map(|_| {
            noise ^= noise << 13;
            noise ^= noise >> 17;
            noise ^= noise << 5;
            noise.to_le_bytes()[0]
        })
        .collect();
    let image = Image::from_u8(side, side, ChannelLayout::Rgba, pixels).unwrap();
    craft_image_codecs::encode(&image, Format::Png, &Default::default()).unwrap()
}

#[test]
fn live_placed_artwork_replicates_from_both_peers_and_replays_after_save() {
    let seed = Document::new(&NewDocument::default());
    let mut host = editor(&seed, "Artwork host");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "host starts", |apps| room(apps[0]).can_edit());
    let invite = room(&host).invitation.clone().unwrap();
    let mut guest = editor(&seed, "Artwork guest");
    join(&mut guest, &invite);
    wait_for(&mut [&mut host, &mut guest], "guest joins", |apps| room(apps[1]).can_edit());

    let files = tempfile::tempdir().unwrap();
    let first = files.path().join("first.png");
    let second = files.path().join("second.png");
    let first_image = artwork_fixture(431);
    let second_image = artwork_fixture(761);
    assert!(first_image.len() > designcraft_collab_core::MAX_PAYLOAD);
    assert!(second_image.len() > designcraft_collab_core::MAX_PAYLOAD);
    std::fs::write(&first, &first_image).unwrap();
    std::fs::write(&second, &second_image).unwrap();

    host.open_picked("place", first.to_str().unwrap().into()).unwrap();
    guest.open_dropped_at(second.to_str().unwrap(), None).unwrap();
    assert!(document(&host).assets.is_empty() && document(&guest).assets.is_empty());
    wait_for(&mut [&mut host, &mut guest], "both fragmented images confirmed", |apps| apps.iter().all(|app| room(app).revision == 2));
    assert_eq!(document(&host), document(&guest));
    for (name, bytes) in [("first.png", &first_image), ("second.png", &second_image)] {
        let asset = document(&guest).assets.values().find(|asset| asset.name == name).unwrap();
        assert_eq!(asset.data.embedded_bytes().unwrap().as_slice(), bytes);
        assert_eq!(asset.pixels, Some((128, 128)));
        assert!(asset.link.is_none() && asset.link_relative.is_none());
    }

    let (frame, expected_asset) = {
        let doc = document(&host);
        let first_asset = doc.assets.values().find(|a| a.name == "first.png").unwrap().id;
        let second_asset = doc.assets.values().find(|a| a.name == "second.png").unwrap().id;
        let frame = doc.spreads[0].items.iter().find(|item| item.graphic().unwrap().asset == first_asset).unwrap().id;
        (frame, second_asset)
    };
    host.run("file.place", json!({"path": second, "frame": frame.0})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "existing artwork is reused", |apps| apps.iter().all(|app| room(app).revision == 3));
    assert_eq!(document(&guest).item(frame).unwrap().graphic().unwrap().asset, expected_asset);
    assert_eq!(document(&guest).assets.len(), 2);
    assert!(room(&host).history.last().unwrap().1.len() < designcraft_collab_core::MAX_PAYLOAD);

    let mut late_guest = DesignApp::new(Session::new(), Services::default());
    late_guest.collaboration.name = "Late artwork guest".into();
    late_guest.collaboration.join_code = invite;
    late_guest.start_seed_download().unwrap();
    wait_for(&mut [&mut host, &mut guest, &mut late_guest], "late guest receives artwork history", |apps| {
        apps[2].collaboration.room.as_ref().is_some_and(|room| room.can_edit() && room.revision == 3)
    });
    assert_eq!(document(&late_guest), document(&host));

    let path = files.path().join("shared.designcraft");
    host.run("file.saveAs", json!({"path": path})).unwrap();
    let mut reopened = DesignApp::new(Session::new(), Services::default());
    reopened.run("file.open", json!({"path": path})).unwrap();
    let confirmed = room(&host).history.iter().map(|(commit, _)| commit.clone()).collect();
    let restored = super::replay::restore(&seed, document(&reopened), confirmed).unwrap();
    assert_eq!(fingerprint(&restored.document).unwrap(), fingerprint(document(&host)).unwrap());
    assert_eq!(restored.document.assets[&expected_asset].data.embedded_bytes().unwrap().as_slice(), &second_image);
}

#[test]
fn placed_pdf_page_and_crop_converge_and_render_after_reopen() {
    use designcraft_doc::{PdfCrop, SpreadRef};
    use designcraft_geom::Point;
    use designcraft_tools::CanvasLayout;

    let mut source = Session::new();
    source.execute("file.new", &json!({"width": 200, "height": 160, "pages": 2, "facingPages": false})).unwrap();
    source.execute("layout.documentSetup", &json!({"bleed": 9})).unwrap();
    let mark = source.execute("frame.create", &json!({"spread": 1, "rect": [40, 40, 90, 90], "content": "unassigned"})).unwrap();
    source.execute("object.fill", &json!({"ids": [mark["id"]], "swatch": "[Black]"})).unwrap();
    let exported = source.execute("file.exportPdf", &json!({"bleed": true})).unwrap();
    let pdf = designcraft_engine::cmd::base64_decode(exported["base64"].as_str().unwrap());

    let initial = Document::new(&NewDocument { facing_pages: false, ..NewDocument::default() });
    let mut host = editor(&initial, "PDF host");
    let trim = host.run("frame.create", json!({"rect": [80, 80, 280, 240], "content": "unassigned"})).unwrap()["id"].as_u64().unwrap();
    let drop = host.run("frame.create", json!({"rect": [325, 80, 543, 258], "content": "unassigned"})).unwrap()["id"].as_u64().unwrap();
    host.run("object.fill", json!({"ids": [drop], "swatch": "[Paper]"})).unwrap();
    let seed = document(&host).clone();
    let mut guest = editor(&seed, "PDF guest");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "PDF room starts", |apps| room(apps[0]).can_edit() && room(apps[0]).invitation.is_some());
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "PDF guest joins", |apps| room(apps[1]).can_edit());

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("artwork.pdf");
    std::fs::write(&file, &pdf).unwrap();
    host.run("selection.set", json!({"ids": [trim]})).unwrap();
    host.run("app.placeDialog", json!({"path": file})).unwrap();
    assert!(document(&host).assets.is_empty());
    let dialog = host.ui.dialog.as_mut().unwrap();
    assert_eq!(dialog.id, "pdfImport");
    assert_eq!(dialog.fields["pages"], 2);
    dialog.fields.insert("page".into(), json!("2"));
    dialog.fields.insert("crop".into(), json!("trim"));

    // The exact bytes inspected by the dialog are held locally, even if the picker path changes.
    std::fs::remove_file(&file).unwrap();
    let shared_uid = host.session.doc().unwrap().uid;
    host.run("file.new", json!({"title": "Another document"})).unwrap();
    crate::dialogs::confirm(&mut host).unwrap();
    assert_eq!(host.session.doc().unwrap().uid, shared_uid);
    assert!(host.session.loaded.is_none());
    wait_for(&mut [&mut host, &mut guest], "placed PDF options confirmed", |apps| apps.iter().all(|app| room(app).revision == 1));

    std::fs::write(&file, &pdf).unwrap();
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
    guest.canvas_rect = Some(rect);
    let view = *guest.view_mut().unwrap();
    let canvas = CanvasLayout::new(document(&guest), false);
    let pointer = crate::canvas::Xf::new(rect, &view).to_screen(canvas.to_canvas(SpreadRef::Doc(0), Point::new(400.0, 145.0)));
    let dropped = guest.open_dropped_at(file.to_str().unwrap(), Some(pointer)).unwrap();
    assert_eq!(dropped["id"], drop);
    wait_for(&mut [&mut host, &mut guest], "same PDF dropped with defaults", |apps| apps.iter().all(|app| room(app).revision == 2));

    host.run("file.place", json!({"path": file, "pdfPage": 2, "x": 80, "y": 360})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "second PDF crop is kept distinct", |apps| apps.iter().all(|app| room(app).revision == 3));
    let history = &room(&host).history;
    assert!(history[0].1.len() > designcraft_collab_core::MAX_PAYLOAD);
    assert!(history[1..].iter().all(|(_, edit)| edit.len() < designcraft_collab_core::MAX_PAYLOAD));
    std::fs::remove_file(&file).unwrap();
    assert_eq!(document(&host), document(&guest));
    let confirmed = document(&guest);
    assert_eq!(confirmed.assets.len(), 3);
    let graphic_asset = |id| &confirmed.assets[&confirmed.item(ItemId(id)).unwrap().graphic().unwrap().asset];
    let trimmed = graphic_asset(trim);
    let default = graphic_asset(drop);
    assert_eq!((trimmed.page, trimmed.pdf_crop), (1, PdfCrop::Trim));
    let (expected_box, _) = designcraft_render::pdf_crop_box(&pdf, 1, PdfCrop::Trim).unwrap();
    assert_eq!(trimmed.shown_box(), Some(expected_box));
    assert_eq!((default.page, default.pdf_crop, default.pdf_box), (0, PdfCrop::Crop, None));
    assert!(confirmed.assets.values().any(|asset| asset.page == 1 && asset.pdf_crop == PdfCrop::Crop && asset.pdf_box.is_none()));
    for asset in confirmed.assets.values() {
        assert_eq!(asset.mime, "application/pdf");
        assert_eq!(asset.data.embedded_bytes().unwrap().as_slice(), pdf);
        assert!(asset.link.is_none() && asset.link_relative.is_none() && asset.proxy.is_none());
    }

    let bundled = super::seed_document::encode(confirmed).unwrap();
    let received = super::seed_document::decode(&bundled).unwrap();
    assert_eq!(fingerprint(&received).unwrap(), fingerprint(confirmed).unwrap());

    let saved = dir.path().join("shared.designcraft");
    host.run("file.saveAs", json!({"path": saved})).unwrap();
    let mut reopened = DesignApp::new(Session::new(), Services::default());
    reopened.run("file.open", json!({"path": saved})).unwrap();
    assert_eq!(fingerprint(document(&reopened)).unwrap(), fingerprint(confirmed).unwrap());
    for app in [&guest, &reopened] {
        let mut renderer = designcraft_render::Renderer::new();
        renderer.threads = 0;
        let page = renderer.render_page(document(app), &app.session.cache, 0, 1.0, false, &Default::default()).unwrap();
        assert!(page.pixel(140, 140)[0] < 64, "selected page contains the mark");
        assert!(page.pixel(390, 150)[0] > 240, "dropped first page is blank");
        assert!(page.pixel(150, 430)[0] < 64, "the second crop also shows the selected page");
    }
}

fn verify_native_save_and_recovery(host: &mut DesignApp, host_id: ItemId) {
    let target = tempfile::tempdir().unwrap();
    let path = target.path().join("Proof.designcraft");
    host.run("file.saveAs", json!({"path": path})).unwrap();
    host.session.set_deferred_edit_durable(true);
    let copy = target.path().join("Snapshot.designcraft");
    assert!(host.run("file.saveAs", json!({"path": copy})).is_err());
    assert!(host.run("file.save", json!({"path": copy})).is_err());
    assert!(!copy.exists());
    host.run("file.saveACopy", json!({"path": copy})).unwrap();
    assert!(copy.exists());
    let saved = host.run("file.save", json!({})).unwrap();
    assert_eq!(saved["path"].as_str(), path.to_str());

    let mut standalone = DesignApp::new(Session::new(), Services::default());
    standalone.run("file.open", json!({"path": path})).unwrap();
    assert_eq!(document(&standalone).item(host_id), document(host).item(host_id));
    let confirmed: Vec<_> = room(host).history.iter().map(|(commit, _)| commit.clone()).collect();
    let recovered = super::replay::restore(&room(host).storage.seed, document(&standalone), confirmed.clone()).unwrap();
    assert_eq!(recovered.document, *document(&standalone));
    assert_eq!(recovered.replica.comments().collect::<Vec<_>>(), room(host).replica.comments().collect::<Vec<_>>());
    let mut incompatible = document(&standalone).clone();
    incompatible.created += 1;
    assert!(super::replay::restore(&room(host).storage.seed, &incompatible, confirmed.clone()).is_err());
    incompatible.created -= 1;
    incompatible.title.push_str(" changed outside the room");
    assert!(super::replay::restore(&room(host).storage.seed, &incompatible, confirmed).is_err());
}

#[test]
fn confirmation_keeps_the_document_within_the_recovery_size_limit() {
    use super::seed_document::{MAX_MODEL_BYTES, model_bytes};

    let mut seed = Document::new(&NewDocument::default());
    let available = MAX_MODEL_BYTES - model_bytes(&seed).unwrap().len();
    seed.title = "a".repeat(available - 8);
    let mut host = editor(&seed, "Host");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "host starts", |apps| room(apps[0]).can_edit());

    let created = host.run("frame.create", json!({"rect": [25, 30, 100, 120], "content": "unassigned"})).unwrap();
    let id = ItemId(created["id"].as_u64().unwrap());
    wait_for(&mut [&mut host], "oversize edit is declined", |apps| room(apps[0]).error.is_some());
    assert!(room(&host).error.as_deref().unwrap().contains("size limit"));
    assert_eq!(room(&host).revision, 0);
    assert!(document(&host).item(id).is_none());
}

#[test]
fn invitation_downloads_embedded_image_and_shares_frame_moves() {
    let (seed, asset, host_id) = embedded_image_fixture();
    let expected_bytes = seed.assets[&asset].data.embedded_bytes().unwrap();
    let original_bounds = seed.item(host_id).unwrap().bounds();
    let mut host = editor(&seed, "Artwork host");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "artwork host starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());

    let mut guest = DesignApp::new(Session::new(), Services::default());
    guest.collaboration.name = "Artwork guest".into();
    guest.collaboration.join_code = room(&host).invitation.clone().unwrap();
    guest.start_seed_download().unwrap();
    wait_for(&mut [&mut host, &mut guest], "guest downloads embedded artwork", |apps| {
        apps[1].collaboration.room.as_ref().is_some_and(Room::can_edit)
    });

    assert_eq!(fingerprint(document(&guest)).unwrap(), fingerprint(&seed).unwrap());
    assert_eq!(document(&guest).assets[&asset].data.embedded_bytes().unwrap(), expected_bytes);
    let shared = room(&host).replica.shared_id(host_id).unwrap();
    let guest_id = room(&guest).replica.local_id(&shared).unwrap();
    assert_eq!(document(&guest).item(guest_id).unwrap().graphic().unwrap().asset, asset);

    guest.run("transform.move", json!({"ids": [guest_id.0], "dx": 36, "dy": 18})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "guest moves embedded image frame", |apps| {
        apps.iter().all(|app| room(app).revision == 1) && room(apps[1]).pending.is_none()
    });
    let moved = document(&host).item(host_id).unwrap().bounds();
    assert!(moved.x0 > original_bounds.x0 && moved.y0 > original_bounds.y0);
    assert_eq!(document(&guest).item(guest_id).unwrap().bounds(), moved);
    assert_eq!(document(&guest).item(guest_id).unwrap().graphic().unwrap().asset, asset);
    assert_eq!(fingerprint(document(&guest)).unwrap(), fingerprint(document(&host)).unwrap());
    assert_eq!(document(&host).assets[&asset].data.embedded_bytes().unwrap(), expected_bytes);
    assert_eq!(document(&guest).assets[&asset].data.embedded_bytes().unwrap(), expected_bytes);
}

#[test]
fn lan_room_replicates_editor_edits_and_replays_comments_to_a_late_guest() {
    // Identical seed archives and in-memory preferences. No profile or Keychain
    // access, no account, no relay and no listener outside the peer transport.
    let seed = Document::new(&NewDocument { facing_pages: false, ..NewDocument::default() });
    let mut host = editor(&seed, "Host");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "host starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    let invitation = room(&host).invitation.clone().unwrap();

    let mut guest = editor(&seed, "Guest");
    join(&mut guest, &invitation);
    wait_for(&mut [&mut host, &mut guest], "guest joins", |apps| room(apps[1]).can_edit());
    assert!(room(&host).peers.values().any(|peer| peer.name == "Guest"));
    assert!(room(&guest).peers.values().any(|peer| peer.name == "Host"));

    let result = host.run("frame.create", json!({"rect": [25, 30, 100, 120], "content": "unassigned"})).unwrap();
    let host_id = ItemId(result["id"].as_u64().unwrap());
    assert!(document(&host).item(host_id).is_none());
    wait_for(&mut [&mut host, &mut guest], "frame creation", |apps| apps.iter().all(|app| room(app).revision == 1));
    let shared = room(&host).replica.shared_id(host_id).unwrap();
    let guest_id = room(&guest).replica.local_id(&shared).unwrap();
    let original = document(&guest).item(guest_id).unwrap().bounds();
    assert_eq!(document(&guest).item(guest_id).unwrap().bounds(), document(&host).item(host_id).unwrap().bounds());

    guest.run("transform.move", json!({"ids": [guest_id.0], "dx": 36, "dy": 18})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "guest transform", |apps| {
        apps.iter().all(|app| room(app).revision == 2) && room(apps[1]).pending.is_none()
    });
    let moved = document(&host).item(host_id).unwrap().bounds();
    assert!(moved.x0 > original.x0 && moved.y0 > original.y0);
    assert_eq!(moved, document(&guest).item(guest_id).unwrap().bounds());

    for (command, revision, expected) in [("edit.undo", 3, original), ("edit.redo", 4, moved)] {
        guest.run(command, json!({})).unwrap();
        wait_for(&mut [&mut host, &mut guest], command, |apps| {
            apps.iter().all(|app| room(app).revision == revision) && room(apps[1]).pending.is_none()
        });
        assert_eq!(document(&host).item(host_id).unwrap().bounds(), expected);
        assert_eq!(document(&guest).item(guest_id).unwrap().bounds(), expected);
    }

    let guest_room = guest.collaboration.room.as_mut().unwrap();
    let thread = SharedId::Created { actor: guest_room.local.as_ref().unwrap().actor.to_string(), sequence: guest_room.next_sequence, index: 0 };
    guest_room
        .queue_local(
            &mut guest.session,
            vec![Operation::CreateComment { id: thread.clone(), anchor: CommentAnchor::Object(shared.clone()), text: "Proof ready".into() }],
        )
        .unwrap();
    wait_for(&mut [&mut host, &mut guest], "guest comment", |apps| apps.iter().all(|app| room(app).revision == 5) && room(apps[1]).pending.is_none());
    let host_room = host.collaboration.room.as_mut().unwrap();
    host_room.queue_local(&mut host.session, vec![Operation::SetResolved { thread, resolved: true }]).unwrap();
    wait_for(&mut [&mut host, &mut guest], "comment resolution", |apps| apps.iter().all(|app| room(app).revision == 6));

    let mut unrelated = seed.clone();
    unrelated.title = "Unrelated local work".into();
    let mut late_guest = editor(&unrelated, "Late joiner");
    late_guest.collaboration.join_code.clone_from(&invitation);
    late_guest.start_seed_download().unwrap();
    wait_for(&mut [&mut host, &mut guest, &mut late_guest], "third peer downloads seed and replays the room", |apps| {
        apps[2].collaboration.room.as_ref().is_some_and(super::Room::can_edit) && apps.iter().all(|app| room(app).revision == 6)
    });
    assert_eq!(late_guest.session.documents().len(), 2);
    assert_eq!(*late_guest.session.documents()[0].doc, unrelated);
    let expected_comments: Vec<_> = room(&host).replica.comments().collect();
    assert_eq!(expected_comments.len(), 1);
    assert!(expected_comments[0].resolved);
    assert_eq!(expected_comments[0].text, "Proof ready");
    assert_eq!(expected_comments[0].anchor, CommentAnchor::Object(shared));
    for replica in [&guest, &late_guest] {
        assert_eq!(document(replica), document(&host));
        assert_eq!(room(replica).replica.comments().collect::<Vec<_>>(), expected_comments);
    }

    verify_native_save_and_recovery(&mut host, host_id);

    host.run("edit.clear", json!({"ids": [host_id.0]})).unwrap();
    wait_for(&mut [&mut host, &mut guest, &mut late_guest], "delete leaves an orphaned thread", |apps| {
        apps.iter().all(|app| room(app).revision == 7)
    });
    for app in [&host, &guest, &late_guest] {
        assert!(document(app).spreads[0].items.is_empty());
        let thread = room(app).replica.comments().next().unwrap();
        assert!(room(app).replica.comment_is_orphaned(document(app), thread));
    }

    let first_page = document(&host).page(0).unwrap().id;
    guest.run("layout.pages.insert", json!({"count": 1})).unwrap();
    wait_for(&mut [&mut host, &mut guest, &mut late_guest], "append blank page", |apps| {
        apps.iter().all(|app| room(app).revision == 8 && document(app).page_count() == 2)
    });
    let added_page = document(&host).page(1).unwrap().id;
    assert_ne!(added_page, first_page);
    assert_eq!(document(&guest).page(1).unwrap().id, added_page);

    host.run("layout.pages.delete", json!({"pages":[1]})).unwrap();
    wait_for(&mut [&mut host, &mut guest, &mut late_guest], "remove blank trailing page", |apps| {
        apps.iter().all(|app| room(app).revision == 9 && document(app).page_count() == 1)
    });
    for app in [&host, &guest, &late_guest] {
        assert_eq!(document(app).page(0).unwrap().id, first_page);
    }
}

#[test]
fn remote_canvas_presence_moves_and_clears_without_editing_the_document() {
    use designcraft_geom::Point;
    use designcraft_tools::CanvasLayout;
    use egui::{Context, Event, Pos2, RawInput, Rect, Shape, vec2};

    let name = "Presence host";
    let initial = Document::new(&NewDocument { facing_pages: false, ..NewDocument::default() });
    let mut host = editor(&initial, name);
    let mut create_frame = |rect| host.run("frame.create", json!({"rect": rect, "content": "unassigned"})).unwrap()["id"].as_u64().unwrap();
    let host_frame = create_frame([100, 130, 230, 240]);
    let guest_frame = create_frame([105, 410, 200, 490]);
    let seed = document(&host).clone();
    let mut guest = editor(&seed, "Presence guest");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "presence host starts", |apps| room(apps[0]).can_edit() && room(apps[0]).invitation.is_some());
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "presence guest joins", |apps| room(apps[1]).can_edit());

    host.run("selection.set", json!({"ids": []})).unwrap();
    guest.run("selection.set", json!({"ids": [guest_frame]})).unwrap();
    let local_selection = guest.session.doc().unwrap().selection.clone();
    assert_eq!(local_selection.items, [ItemId(guest_frame)]);

    let host_ctx = Context::default();
    let guest_ctx = Context::default();
    let host_size = vec2(1440.0, 900.0);
    let guest_size = vec2(1200.0, 760.0);
    let draw_frame = |app: &mut DesignApp, ctx: &Context, size, event: Option<Event>| {
        let rect = Rect::from_min_size(Pos2::ZERO, size);
        let raw = RawInput { screen_rect: Some(rect), events: event.into_iter().collect(), ..Default::default() };
        app.poll_collaboration(ctx);
        let mut output = ctx.run_ui(raw, |ui| crate::canvas::show(app, ui));
        output.textures_delta.clear();
        output
    };
    for _ in 0..3 {
        draw_frame(&mut host, &host_ctx, host_size, None);
        draw_frame(&mut guest, &guest_ctx, guest_size, None);
    }

    let slot = CanvasLayout::new(&seed, false).slots[0];
    let screen = |app: &DesignApp| crate::canvas::Xf::new(app.canvas_rect.unwrap(), app.view().unwrap());
    let host_screen = screen(&host);
    let guest_screen = screen(&guest);
    let bounds = guest_screen.rect(slot.xf.transform_rect_bbox(seed.item(ItemId(host_frame)).unwrap().bounds()));
    let first = slot.to_canvas(Point::new(315.0, 260.0));
    let second = slot.to_canvas(Point::new(410.0, 335.0));
    assert_ne!(host_screen.to_screen(first), guest_screen.to_screen(first));
    let actor_color = super::presence::color(room(&host).local.as_ref().unwrap().actor);
    let wait_for_paint = |host: &mut DesignApp, guest: &mut DesignApp, event: Event, expected: Option<Pos2>, selected| {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut event = Some(event);
        loop {
            draw_frame(host, &host_ctx, host_size, event.take());
            let output = draw_frame(guest, &guest_ctx, guest_size, None);
            let mut cursor = None;
            let mut label = false;
            let mut outline = false;
            for clipped in output.shapes {
                match clipped.shape {
                    Shape::Path(path) if path.fill == actor_color && path.closed => {
                        cursor = path.points.first().copied().filter(|tip| clipped.clip_rect.contains(*tip));
                    }
                    Shape::Text(text) if text.galley.text() == name => label = clipped.clip_rect.contains(text.pos),
                    Shape::Rect(rect) if rect.stroke.color == actor_color => {
                        outline |= (rect.rect.min.distance(bounds.min) < 0.5 && rect.rect.max.distance(bounds.max) < 0.5)
                            && clipped.clip_rect.contains_rect(rect.rect);
                    }
                    _ => {}
                }
            }
            let cursor_matches = match (cursor, expected) {
                (Some(actual), Some(expected)) => actual.distance(expected) < 0.5,
                (None, None) => true,
                _ => false,
            };
            if cursor_matches && label == expected.is_some() && outline == selected {
                break;
            }
            assert!(Instant::now() < deadline, "cursor {cursor:?}, label {label}, outline {outline}; expected {expected:?}, selected {selected}");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(document(host), &seed);
        assert_eq!(document(guest), &seed);
        assert_eq!(guest.session.doc().unwrap().selection, local_selection);
        assert_eq!((room(host).revision, room(guest).revision), (0, 0));
        assert!(room(host).history.is_empty() && room(guest).history.is_empty());
    };

    host.run("selection.set", json!({"ids": [host_frame]})).unwrap();
    wait_for_paint(&mut host, &mut guest, Event::PointerMoved(host_screen.to_screen(first)), Some(guest_screen.to_screen(first)), true);
    host.run("selection.set", json!({"ids": []})).unwrap();
    wait_for_paint(&mut host, &mut guest, Event::PointerMoved(host_screen.to_screen(second)), Some(guest_screen.to_screen(second)), false);
    wait_for_paint(&mut host, &mut guest, Event::PointerGone, None, false);
}

fn wait_for_layer_revision(host: &mut DesignApp, guest: &mut DesignApp, stage: &str, revision: u64) {
    wait_for(&mut [host, guest], stage, |apps| apps.iter().all(|app| room(app).revision == revision));
}

#[test]
fn concurrent_layers_keep_canonical_ids_and_peer_local_activation() {
    let seed = Document::new(&NewDocument::default());
    let base = seed.working_layer();
    let mut host = editor(&seed, "Layers host");
    let mut guest = editor(&seed, "Layers guest");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "layers host starts", |apps| room(apps[0]).can_edit() && room(apps[0]).invitation.is_some());
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "layers guest joins", |apps| room(apps[1]).can_edit());

    let host_draft = host.run("layer.new", json!({"name": "Artwork"})).unwrap();
    let guest_draft = guest.run("layer.new", json!({"name": "Proof"})).unwrap();
    assert_eq!(host_draft, guest_draft, "both peers allocated the same draft ID");
    assert_eq!(host.session.doc().unwrap().active_layer, base);
    assert_eq!(guest.session.doc().unwrap().active_layer, base);
    wait_for_layer_revision(&mut host, &mut guest, "both layers are confirmed", 2);
    assert_eq!(document(&host), document(&guest));
    let artwork = document(&host).layers.iter().find(|layer| layer.name == "Artwork").unwrap().id;
    let proof = document(&guest).layers.iter().find(|layer| layer.name == "Proof").unwrap().id;
    assert_ne!(artwork, proof);
    assert_eq!(host.session.doc().unwrap().active_layer, artwork);
    assert_eq!(guest.session.doc().unwrap().active_layer, proof);

    host.run("frame.create", json!({"rect": [25, 35, 110, 125], "content": "unassigned"})).unwrap();
    guest.run("frame.create", json!({"rect": [150, 45, 220, 130], "content": "unassigned"})).unwrap();
    wait_for_layer_revision(&mut host, &mut guest, "both drawings are confirmed", 4);
    assert_eq!(document(&host), document(&guest));
    let items = &document(&host).spreads[0].items;
    let artwork_frame = items.iter().find(|item| item.layer == artwork).unwrap().id;
    let proof_frame = items.iter().find(|item| item.layer == proof).unwrap().id;
    let proof_identity = room(&guest).replica.shared_id(proof_frame).unwrap();
    let proof_bounds = document(&guest).item(proof_frame).unwrap().bounds();

    host.run("layer.set", json!({"id": artwork.0, "name": "Locked artwork", "locked": true, "visible": false, "printable": false})).unwrap();
    wait_for_layer_revision(&mut host, &mut guest, "layer options", 5);
    let received = document(&guest).layer(artwork).unwrap();
    assert_eq!(received.name, "Locked artwork");
    assert!(received.locked && !received.visible && !received.printable);

    guest.run("layer.move", json!({"id": proof.0, "to": 0})).unwrap();
    wait_for_layer_revision(&mut host, &mut guest, "shared layer order", 6);
    assert_eq!(document(&host).layers[0].id, proof);
    assert_eq!(host.session.doc().unwrap().active_layer, artwork);
    assert_eq!(guest.session.doc().unwrap().active_layer, proof);

    guest.run("layer.new", json!({"name": "Spare"})).unwrap();
    guest.run("layer.activate", json!({"id": base.0})).unwrap();
    wait_for_layer_revision(&mut host, &mut guest, "later local activation survives confirmation", 7);
    let spare = document(&guest).layers.iter().find(|layer| layer.name == "Spare").unwrap().id;
    assert_eq!(guest.session.doc().unwrap().active_layer, base);
    guest.run("layer.activate", json!({"id": proof.0})).unwrap();

    host.run("layer.merge", json!({"ids": [proof.0, base.0], "into": base.0})).unwrap();
    wait_for_layer_revision(&mut host, &mut guest, "layer merge", 8);
    assert_eq!(guest.session.doc().unwrap().active_layer, spare);
    assert_eq!(room(&guest).replica.local_id(&proof_identity), Some(proof_frame));
    let moved = document(&guest).item(proof_frame).unwrap();
    assert_eq!(moved.layer, base);
    assert_eq!(moved.bounds(), proof_bounds);

    verify_layer_removal_and_copy(&mut host, &mut guest, artwork, artwork_frame, spare, base);
    let confirmed = room(&host).history.iter().map(|(commit, _)| commit.clone()).collect();
    let replayed = super::replay::restore(&seed, &seed, confirmed).unwrap();
    assert_eq!(&replayed.document, document(&host));
}

fn verify_layer_removal_and_copy(
    host: &mut DesignApp,
    guest: &mut DesignApp,
    artwork: LayerId,
    artwork_frame: ItemId,
    spare: LayerId,
    base: LayerId,
) {
    let host_room = host.collaboration.room.as_mut().unwrap();
    let artwork_identity = host_room.replica.shared_id(artwork_frame).unwrap();
    let id = SharedId::Created { actor: host_room.local.as_ref().unwrap().actor.to_string(), sequence: host_room.next_sequence, index: 0 };
    host_room
        .queue_local(
            &mut host.session,
            vec![Operation::CreateComment { id, anchor: CommentAnchor::Object(artwork_identity.clone()), text: "Art review".into() }],
        )
        .unwrap();
    wait_for_layer_revision(host, guest, "layer artwork comment", 9);
    guest.run("layer.delete", json!({"id": artwork.0})).unwrap();
    wait_for_layer_revision(host, guest, "occupied layer removal", 10);
    assert_eq!(host.session.doc().unwrap().active_layer, spare);
    assert_eq!(document(host), document(guest));
    for app in [&*host, &*guest] {
        let replica = &room(app).replica;
        assert!(replica.local_id(&artwork_identity).is_none());
        assert!(replica.comment_is_orphaned(document(app), replica.comments().next().unwrap()));
    }

    host.run("layer.deleteUnused", json!({})).unwrap();
    wait_for_layer_revision(host, guest, "clear empty layers", 11);
    assert_eq!(host.session.doc().unwrap().active_layer, base);
    assert_eq!(guest.session.doc().unwrap().active_layer, base);
    assert_eq!(document(host), document(guest));
    assert_eq!(document(host).layers.len(), 1);

    guest.run("layer.new", json!({"name": "Overlay"})).unwrap();
    wait_for_layer_revision(host, guest, "empty overlay", 12);
    let overlay = document(guest).layers[0].id;
    guest.run("layer.duplicate", json!({"id": overlay.0})).unwrap();
    wait_for_layer_revision(host, guest, "duplicate an empty layer", 13);
    let copied = document(host).layers[0].id;
    assert_ne!(copied, overlay);
    assert_eq!(document(guest).layer(copied).unwrap().name, "Overlay copy");
    assert_eq!(guest.session.doc().unwrap().active_layer, copied);
    host.run("layer.others", json!({"id": base.0, "hide": true, "lock": true})).unwrap();
    wait_for_layer_revision(host, guest, "hide and lock the other layers", 14);
    assert_eq!(document(host), document(guest));
    assert!(document(guest).layers.iter().filter(|layer| layer.id != base).all(|layer| layer.locked && !layer.visible));
}

#[test]
fn facing_page_edits_keep_page_selection_artwork_and_comment_anchors() {
    let seed = Document::new(&NewDocument { pages: 4, facing_pages: true, ..NewDocument::default() });
    let page_index = 2;
    let original_page = seed.page(page_index).unwrap();
    let (spread, _) = seed.page_loc(page_index).unwrap();
    let (page_id, page_x) = (original_page.id, original_page.x);
    let mut host = editor(&seed, "Layout host");
    let created =
        host.run("frame.create", json!({"spread": spread, "rect": [page_x + 25.0, 40.0, page_x + 125.0, 120.0], "content": "unassigned"})).unwrap();
    let frame = ItemId(created["id"].as_u64().unwrap());
    let seed = document(&host).clone();
    let page_relative_bounds = |doc: &Document, index| {
        let (spread, slot) = doc.page_loc(index).unwrap();
        let bounds = doc.item(frame).unwrap().bounds();
        assert_eq!(doc.spreads[spread].page_at_x(bounds.center().x), Some(slot));
        [bounds.x0 - doc.page(index).unwrap().x, bounds.y0, bounds.width(), bounds.height()]
    };
    let original_bounds = page_relative_bounds(&seed, page_index);
    let page_ids = |doc: &Document| doc.spreads.iter().flat_map(|spread| spread.pages.iter().map(|page| page.id)).collect::<Vec<_>>();
    let mut original_ids = page_ids(&seed);

    let mut guest = editor(&seed, "Layout guest");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "layout host starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "layout guest joins", |apps| room(apps[1]).can_edit());
    guest.run("layout.selectPage", json!({"page": page_index + 1, "ids": [frame.0]})).unwrap();
    let shared_frame = room(&host).replica.shared_id(frame).unwrap();

    let host_room = host.collaboration.room.as_mut().unwrap();
    let actor = host_room.local.as_ref().unwrap().actor.to_string();
    let sequence = host_room.next_sequence;
    let comments = [
        Operation::CreateComment {
            id: SharedId::Created { actor: actor.clone(), sequence, index: 0 },
            anchor: CommentAnchor::Page(page_id),
            text: "Review this page".into(),
        },
        Operation::CreateComment {
            id: SharedId::Created { actor, sequence, index: 1 },
            anchor: CommentAnchor::Object(shared_frame.clone()),
            text: "Check this frame".into(),
        },
    ];
    host_room.queue_local(&mut host.session, comments.into()).unwrap();
    wait_for(&mut [&mut host, &mut guest], "page and object comments received", |apps| apps.iter().all(|app| room(app).revision == 1));
    let original_comments = room(&host).replica.comments().cloned().collect::<Vec<_>>();

    host.run("layout.pages.insert", json!({"before": page_index, "count": 1})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "insert before occupied facing page", |apps| apps.iter().all(|app| room(app).revision == 2));
    assert_eq!(document(&host), document(&guest));
    let mut inserted_ids = page_ids(document(&guest));
    let inserted_id = inserted_ids.remove(page_index);
    assert!(!original_ids.contains(&inserted_id));
    assert_eq!(inserted_ids, original_ids);
    let moved_page = document(&host).page(page_index + 1).unwrap();
    assert_eq!(moved_page.id, page_id);
    assert_ne!(moved_page.x, page_x, "the original page moved to the other side of the spread");
    assert_eq!(page_relative_bounds(document(&guest), page_index + 1), original_bounds);
    assert_eq!(room(&guest).replica.local_id(&shared_frame), Some(frame));
    assert_eq!(guest.session.doc().unwrap().selection.page, Some(page_index + 1));
    assert_eq!(guest.session.doc().unwrap().selection.items, [frame]);
    assert!(room(&guest).replica.comments().all(|thread| !room(&guest).replica.comment_is_orphaned(document(&guest), thread)));

    guest.run("layout.pages.delete", json!({"pages": [page_index]})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "remove inserted page", |apps| apps.iter().all(|app| room(app).revision == 3));
    assert_eq!(document(&host), document(&guest));
    assert_eq!(page_ids(document(&guest)), original_ids);
    assert_eq!(page_relative_bounds(document(&guest), page_index), original_bounds);
    assert_eq!(guest.session.doc().unwrap().selection.page, Some(page_index));

    host.run("layout.pages.delete", json!({"pages": [page_index]})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "remove occupied facing page", |apps| apps.iter().all(|app| room(app).revision == 4));
    assert_eq!(document(&host), document(&guest));
    assert!(guest.session.doc().unwrap().selection.page.is_none());
    assert!(guest.session.doc().unwrap().selection.items.is_empty());
    original_ids.remove(page_index);
    for app in [&host, &guest] {
        assert_eq!(page_ids(document(app)), original_ids);
        assert!(document(app).item(frame).is_none());
        assert_eq!(room(app).replica.local_id(&shared_frame), None);
        let comments = room(app).replica.comments().cloned().collect::<Vec<_>>();
        assert_eq!(comments, original_comments);
        assert!(comments.iter().all(|thread| room(app).replica.comment_is_orphaned(document(app), thread)));
    }
}

#[test]
fn three_peers_converge_after_delayed_reordered_and_duplicate_packets() {
    use super::{Message, Mode};

    let mut host = editor(&Document::new(&NewDocument::default()), "Test host");
    let shape = host.run("frame.create", json!({"rect": [25, 30, 100, 120], "content": "unassigned"})).unwrap();
    let shape = ItemId(shape["id"].as_u64().unwrap());
    let text = host.run("frame.create", json!({"rect": [120, 50, 300, 200], "content": "text", "text": "ABCD"})).unwrap();
    let text_frame = ItemId(text["id"].as_u64().unwrap());
    let story = StoryId(text["story"].as_u64().unwrap());
    let seed = document(&host).clone();
    let mut writer = editor(&seed, "Test writer");
    let mut reviewer = editor(&seed, "Test reviewer");

    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "host starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    let invite = room(&host).invitation.clone().unwrap();
    join(&mut writer, &invite);
    join(&mut reviewer, &invite);
    wait_for(&mut [&mut host, &mut writer, &mut reviewer], "all peers join revision zero", |apps| {
        apps.iter().all(|app| room(app).can_edit() && room(app).revision == 0)
    });

    host.run("transform.move", json!({"ids": [shape.0], "dx": 36, "dy": 18})).unwrap();
    writer.run("text.select", json!({"story": story.0, "anchor": 2, "focus": 2})).unwrap();
    writer.run("text.insert", json!({"text": "λ", "raw": true})).unwrap();
    reviewer.run("object.fill", json!({"ids": [shape.0], "swatch": "[Black]"})).unwrap();

    // Stage all three changes before polling transport. Send the fill before the delayed text proposal.
    for app in [&mut host, &mut writer, &mut reviewer] {
        let edit = app.session.take_deferred_edit().unwrap();
        app.collaboration.room.as_mut().unwrap().capture_edit(&mut app.session, edit).unwrap();
    }
    let fill_proposal = room(&reviewer).pending.clone().unwrap();
    let text_proposal = room(&writer).pending.clone().unwrap();
    deliver_room_packet(&reviewer, &mut host, Message::Propose(fill_proposal.clone()));
    deliver_room_packet(&reviewer, &mut host, Message::Propose(fill_proposal));
    assert_eq!(room(&host).revision, 2);
    deliver_room_packet(&writer, &mut host, Message::Propose(text_proposal));
    assert_eq!(room(&host).revision, 3);
    let history = &room(&host).history;
    assert!(history.iter().all(|(commit, _)| commit.proposal.base == 0));

    // A real revision arrives before its predecessors. The reviewer must request the gap
    // and keep its own proposal pending until confirmation. The writer gets revision one twice.
    let moved = history[0].0.clone();
    let typed = history[2].0.clone();
    deliver_room_packet(&host, &mut reviewer, Message::Commit(typed.clone()));
    assert_eq!(document(&reviewer), &seed);
    assert_eq!(room(&reviewer).revision, 0);
    assert!(room(&reviewer).pending.is_some());
    assert!(matches!(room(&reviewer).mode, Mode::Guest { ready: false, hello_sent: false }));
    deliver_room_packet(&host, &mut writer, Message::Commit(moved.clone()));
    deliver_room_packet(&host, &mut writer, Message::Commit(moved));
    assert_eq!(room(&writer).history.len(), 1);
    assert_eq!(document(&writer).story(story).unwrap().text, "ABCD");

    wait_for(&mut [&mut host, &mut writer, &mut reviewer], "three peers fill the gap without reconnecting", |apps| {
        apps.iter().all(|app| room(app).online && room(app).can_edit() && room(app).revision == 3)
    });
    deliver_room_packet(&host, &mut writer, Message::Commit(typed));
    assert!(!room(&writer).fatal);
    assert!(writer.session.doc().unwrap().selection.text.is_some());
    assert!(reviewer.session.doc().unwrap().selection.text.is_none());

    // Continue over the original transport after fault injection.
    let anchor = CommentAnchor::Object(SharedId::Seed(shape));
    let review_room = reviewer.collaboration.room.as_mut().unwrap();
    let thread = SharedId::Created { actor: review_room.local.as_ref().unwrap().actor.to_string(), sequence: review_room.next_sequence, index: 0 };
    review_room
        .queue_local(
            &mut reviewer.session,
            vec![Operation::CreateComment { id: thread.clone(), anchor: anchor.clone(), text: "Check alignment".into() }],
        )
        .unwrap();
    wait_for(&mut [&mut host, &mut writer, &mut reviewer], "comment converges over the active room", |apps| {
        apps.iter().all(|app| room(app).can_edit() && room(app).revision == 4)
    });
    let state = writer.collaboration.room.as_mut().unwrap();
    let reply = SharedId::Created { actor: state.local.as_ref().unwrap().actor.to_string(), sequence: state.next_sequence, index: 0 };
    state.queue_local(&mut writer.session, vec![Operation::Reply { thread: thread.clone(), id: reply, text: "Aligned".into() }]).unwrap();
    wait_for(&mut [&mut host, &mut writer, &mut reviewer], "reply converges over the active room", |apps| {
        apps.iter().all(|app| room(app).can_edit() && room(app).revision == 5)
    });

    let host_doc = document(&host);
    assert_converged_layout(host_doc, &seed, shape, text_frame, story);
    let expected_document = fingerprint(host_doc).unwrap();
    let expected_comments: Vec<_> = room(&host).replica.comments().collect();
    assert_eq!(expected_comments.len(), 1);
    let comment = expected_comments[0];
    assert_eq!(comment.id, thread);
    assert_eq!(comment.anchor, anchor);
    assert_eq!(comment.text, "Check alignment");
    assert_eq!(comment.replies.len(), 1);
    assert_eq!(comment.replies[0].text, "Aligned");
    for app in [&writer, &reviewer] {
        assert_eq!(fingerprint(document(app)).unwrap(), expected_document);
        assert_eq!(room(app).replica.comments().collect::<Vec<_>>(), expected_comments);
    }
    assert!([&host, &writer, &reviewer].iter().all(|app| !room(app).replica.comment_is_orphaned(document(app), comment)));
}

#[test]
fn shared_text_comment_tracks_original_atoms_through_edits_and_replay() {
    use std::ops::Range;

    use designcraft_collab::text::{AtomId, MAX_EDIT_ATOMS};

    let mut host = editor(&Document::new(&NewDocument::default()), "Text host");
    let created = host.run("frame.create", json!({"rect": [40, 50, 300, 200], "content": "text", "text": "AéβZ"})).unwrap();
    let frame = created["id"].as_u64().unwrap();
    let story = StoryId(created["story"].as_u64().unwrap());
    let seed = document(&host).clone();
    let mut guest = editor(&seed, "Text reviewer");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "text host starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "text reviewer joins", |apps| room(apps[1]).can_edit());

    let anchor = room(&guest).replica.text_comment_anchor(document(&guest), story, 1..5).unwrap();
    assert_eq!(anchor, CommentAnchor::Text { story, atoms: vec![AtomId::Seed(1), AtomId::Seed(2)] });
    let review_room = guest.collaboration.room.as_mut().unwrap();
    let thread = SharedId::Created { actor: review_room.local.as_ref().unwrap().actor.to_string(), sequence: review_room.next_sequence, index: 0 };
    review_room
        .queue_local(
            &mut guest.session,
            vec![Operation::CreateComment { id: thread.clone(), anchor: anchor.clone(), text: "Check this text".into() }],
        )
        .unwrap();

    // Confirm the concurrent insertion first, so the host receives the comment from revision zero.
    host.run("text.select", json!({"story": story.0, "anchor": 3})).unwrap();
    host.run("text.insert", json!({"text": "λ", "raw": true})).unwrap();
    let insertion = host.session.take_deferred_edit().unwrap();
    host.collaboration.room.as_mut().unwrap().capture_edit(&mut host.session, insertion).unwrap();
    assert_eq!(room(&host).revision, 1);
    wait_for(&mut [&mut host, &mut guest], "stale text comment confirmed", |apps| apps.iter().all(|app| room(app).revision == 2));
    assert_eq!(room(&host).history[1].0.proposal.base, 0);

    let assert_range = |host: &DesignApp, guest: &DesignApp, text: Option<&str>, range: Option<Range<usize>>| {
        assert_eq!(document(host), document(guest));
        for app in [host, guest] {
            let replica = &room(app).replica;
            let comment = replica.comments().find(|comment| comment.id == thread).unwrap();
            assert_eq!(comment.anchor, anchor);
            assert_eq!(comment.text, "Check this text");
            assert_eq!(document(app).story(story).map(|story| story.text.as_str()), text);
            assert_eq!(replica.comment_text_range(document(app), &comment.anchor), range.clone().map(|range| (story, range)));
            assert_eq!(replica.comment_is_orphaned(document(app), comment), range.is_none());
        }
    };
    assert_range(&host, &guest, Some("AéλβZ"), Some(1..7));

    for (deleted, revision, text, range) in [(1..3, 3, "AλβZ", Some(3..5)), (3..5, 4, "AλZ", None)] {
        host.run("text.select", json!({"story": story.0, "anchor": deleted.start, "focus": deleted.end})).unwrap();
        host.run("text.delete", json!({})).unwrap();
        wait_for(&mut [&mut host, &mut guest], "delete original comment text", |apps| apps.iter().all(|app| room(app).revision == revision));
        assert_range(&host, &guest, Some(text), range);
    }
    for (revision, text, range) in [(5, "AλβZ", 3..5), (6, "AéλβZ", 1..7)] {
        host.run("edit.undo", json!({})).unwrap();
        wait_for(&mut [&mut host, &mut guest], "undo revives the text anchor", |apps| apps.iter().all(|app| room(app).revision == revision));
        assert_range(&host, &guest, Some(text), Some(range));
    }

    let confirmed: Vec<_> = room(&host).history.iter().map(|(commit, _)| commit).collect();
    let saved_commits = serde_json::to_vec(&confirmed).unwrap();
    let recovered = super::replay::restore(&seed, &seed, serde_json::from_slice(&saved_commits).unwrap()).unwrap();
    assert_eq!(&recovered.document, document(&host));
    assert_eq!(recovered.replica.comments().collect::<Vec<_>>(), room(&host).replica.comments().collect::<Vec<_>>());
    assert_eq!(recovered.replica.comment_text_range(&recovered.document, &anchor), Some((story, 1..7)));

    let start = "AéλβZ".len();
    host.run("text.select", json!({"story": story.0, "anchor": start})).unwrap();
    host.run("text.insert", json!({"text": "x".repeat(MAX_EDIT_ATOMS), "raw": true})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "the longest supported selection is inserted", |apps| apps.iter().all(|app| room(app).revision == 7));
    let range = start..start + MAX_EDIT_ATOMS;
    let large_anchor = room(&guest).replica.text_comment_anchor(document(&guest), story, range.clone()).unwrap();
    let review_room = guest.collaboration.room.as_mut().unwrap();
    let long_id = SharedId::Created { actor: review_room.local.as_ref().unwrap().actor.to_string(), sequence: review_room.next_sequence, index: 0 };
    review_room
        .queue_local(
            &mut guest.session,
            vec![Operation::CreateComment { id: long_id.clone(), anchor: large_anchor.clone(), text: "Review this passage".into() }],
        )
        .unwrap();
    assert!(review_room.sending_proposal.as_ref().unwrap().0.len() > designcraft_collab_core::MAX_PAYLOAD);
    wait_for(&mut [&mut host, &mut guest], "the large text anchor crosses the transport in chunks", |apps| {
        apps.iter().all(|app| room(app).revision == 8)
    });
    for app in [&host, &guest] {
        let replica = &room(app).replica;
        let comment = replica.comments().find(|comment| comment.id == long_id).unwrap();
        assert_eq!(comment.anchor, large_anchor);
        assert_eq!(replica.comment_text_range(document(app), &comment.anchor), Some((story, range.clone())));
    }

    let review_room = guest.collaboration.room.as_mut().unwrap();
    let rejected = SharedId::Created { actor: review_room.local.as_ref().unwrap().actor.to_string(), sequence: review_room.next_sequence, index: 0 };
    review_room
        .queue_local(&mut guest.session, vec![Operation::CreateComment { id: rejected, anchor: anchor.clone(), text: "Keep this draft".into() }])
        .unwrap();
    host.run("edit.clear", json!({"ids": [frame]})).unwrap();
    let deletion = host.session.take_deferred_edit().unwrap();
    host.collaboration.room.as_mut().unwrap().capture_edit(&mut host.session, deletion).unwrap();
    wait_for(&mut [&mut host, &mut guest], "story removal and stale comment rejection", |apps| {
        apps.iter().all(|app| room(app).revision == 9) && room(apps[1]).pending.is_none()
    });
    assert_range(&host, &guest, None, None);
    assert_eq!(guest.collaboration.comment, "Keep this draft");
    assert!(guest.collaboration.comment_needs_text_selection);
}

#[test]
fn share_window_posts_and_reselects_text_without_editing_the_story() {
    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };

    let mut app = editor(&Document::new(&NewDocument::default()), "Text reviewer");
    let text = format!("Aéβ{}", "Z".repeat(520));
    let frame = app.run("frame.create", json!({"rect": [40, 50, 300, 200], "content": "text", "text": text})).unwrap();
    let story = StoryId(frame["story"].as_u64().unwrap());
    app.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut app], "text comment composer is ready", |apps| room(apps[0]).can_edit());
    app.open_collaboration_dialog();
    app.run("text.select", json!({"story": story.0, "anchor": 0, "focus": text.len()})).unwrap();
    app.collaboration.comment = "Check this passage".into();

    let mut harness =
        Harness::builder().with_size(egui::vec2(550.0, 620.0)).build_ui_state(|ui, app: &mut DesignApp| app.show_collaboration(ui.ctx()), app);
    harness.run_steps(3);
    harness.get_by_label("This text selection can’t be anchored.");
    assert!(harness.get_by_label("Post").accesskit_node().is_disabled());
    assert_eq!(room(harness.state()).revision, 0);
    assert_eq!(harness.state().collaboration.comment, "Check this passage");

    let app = harness.state_mut();
    app.collaboration.comment_needs_text_selection = true;
    app.run("text.select", json!({"story": story.0, "anchor": 0, "focus": 0})).unwrap();
    harness.run_steps(3);
    harness.get_by_label("Select text to reattach this comment.");
    assert!(harness.get_by_label("Post").accesskit_node().is_disabled());

    harness.state_mut().run("text.select", json!({"story": story.0, "anchor": 5, "focus": 1})).unwrap();
    harness.run_steps(3);
    harness.get_by_label("Attached to selected text");
    harness.get_by_label("Post").click();
    harness.run_steps(3);
    let app = harness.state_mut();
    assert_eq!(room(app).revision, 1);
    assert_eq!(room(app).replica.comments().next().unwrap().text, "Check this passage");
    app.run("text.select", json!({"story": story.0, "anchor": 0, "focus": 0})).unwrap();
    harness.run_steps(3);
    harness.get_by_label("Select text").click();
    harness.run_steps(3);
    let app = harness.state();
    assert_eq!(app.session.active().unwrap().selection.text.unwrap().range(), 1..5);
    assert_eq!(document(app).story(story).unwrap().text, text);
    assert_eq!(room(app).revision, 1);
}

#[test]
fn concurrent_text_frame_creations_keep_their_carets_and_replicate_edits() {
    let seed = Document::new(&NewDocument::default());
    let mut host = editor(&seed, "Text host");
    let mut guest = editor(&seed, "Text guest");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "text host starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "text guest joins", |apps| room(apps[1]).can_edit());

    // Both creations are proposed from the same revision and use the same initial allocator state.
    for (app, text, rect) in [(&mut host, "Café", [40, 50, 300, 200]), (&mut guest, "Ω proof", [40, 220, 300, 370])] {
        app.run("frame.create", json!({"rect": rect, "content": "text", "text": text, "caret": true})).unwrap();
        assert_eq!(document(app), &seed);
    }
    wait_for(&mut [&mut host, &mut guest], "both text frames confirmed", |apps| apps.iter().all(|app| room(app).revision == 2));
    assert_eq!(document(&host), document(&guest));

    let [host_caret, guest_caret] = [&host, &guest].map(|app| app.session.doc().unwrap().selection.text.unwrap());
    assert_ne!(host_caret.story, guest_caret.story);
    assert_ne!(host_caret.frame, guest_caret.frame);
    for (app, peer, caret, initial) in [(&host, &guest, host_caret, "Café"), (&guest, &host, guest_caret, "Ω proof")] {
        assert!(caret.is_caret());
        assert_eq!(caret.focus, initial.len());
        let frame = caret.frame.unwrap();
        let local_story = document(app).item(frame).unwrap().text_frame().unwrap().story;
        assert_eq!(local_story, caret.story);
        assert_eq!(document(app).story(local_story).unwrap().text, initial);

        let shared_frame = room(app).replica.shared_id(frame).unwrap();
        let remote_frame = room(peer).replica.local_id(&shared_frame).unwrap();
        let remote_story = document(peer).item(remote_frame).unwrap().text_frame().unwrap().story;
        assert_eq!(document(peer).story(remote_story).unwrap().text, initial);
    }

    // Typing immediately after confirmation must use the author's remapped caret.
    host.run("text.insert", json!({"text": "!", "raw": true})).unwrap();
    guest.run("text.insert", json!({"text": "…", "raw": true})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "both new stories edited", |apps| apps.iter().all(|app| room(app).revision == 4));
    assert_eq!(document(&host), document(&guest));
    assert_eq!(document(&host).story(host_caret.story).unwrap().text, "Café!");
    assert_eq!(document(&host).story(guest_caret.story).unwrap().text, "Ω proof…");
}

#[test]
fn typing_before_a_frame_conversion_is_confirmed_reaches_the_new_story() {
    let mut host = editor(&Document::new(&NewDocument::default()), "Text host");
    let created = host.run("frame.create", json!({"rect": [40, 50, 300, 200], "content": "unassigned"})).unwrap();
    let frame = ItemId(created["id"].as_u64().unwrap());
    let earlier = host.run("frame.create", json!({"rect": [40, 230, 300, 380], "content": "text", "text": "Previous story"})).unwrap();
    let earlier_story = designcraft_doc::StoryId(earlier["story"].as_u64().unwrap());
    let seed = document(&host).clone();
    let mut guest = editor(&seed, "Text guest");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "host starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "guest joins", |apps| room(apps[1]).can_edit());

    guest.run("text.select", json!({"story": earlier_story.0, "anchor": 0})).unwrap();
    guest.run("text.placeCaret", json!({"frame": frame.0, "point": [60, 70]})).unwrap();
    assert!(document(&guest).item(frame).unwrap().text_frame().is_none());
    assert!(guest.buffer_shared_typing("Résumé α"));
    wait_for(&mut [&mut host, &mut guest], "converted frame and queued typing confirmed", |apps| apps.iter().all(|app| room(app).revision == 2));
    assert_eq!(document(&host), document(&guest));
    let text_frame = document(&host).item(frame).unwrap().text_frame().unwrap();
    assert_eq!(document(&host).story(text_frame.story).unwrap().text, "Résumé α");
    assert_eq!(document(&host).story(earlier_story).unwrap().text, "Previous story");
    let caret = guest.session.doc().unwrap().selection.text.unwrap();
    assert_eq!(caret.story, text_frame.story);
    assert_eq!(caret.frame, Some(frame));
    assert_eq!(caret.focus, "Résumé α".len());

    let confirmed = room(&host).history.iter().map(|(commit, _)| commit.clone()).collect();
    let recovered = super::replay::restore(&seed, &seed, confirmed).unwrap();
    assert_eq!(&recovered.document, document(&host));
}

#[test]
fn deleting_a_shared_text_frame_preserves_declined_and_buffered_typing() {
    let mut host = editor(&Document::new(&NewDocument::default()), "Text host");
    let created = host.run("frame.create", json!({"rect": [40, 50, 300, 200], "content": "text", "text": "Proof"})).unwrap();
    let frame = created["id"].as_u64().unwrap();
    let story = created["story"].as_u64().unwrap();
    let mut guest = editor(document(&host), "Text guest");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "text host starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "text guest joins", |apps| room(apps[1]).can_edit());

    let submitted = "🧪".repeat(151);
    guest.run("text.select", json!({"story": story, "anchor": 5})).unwrap();
    guest.run("text.insert", json!({"text": submitted, "raw": true})).unwrap();
    let edit = guest.session.take_deferred_edit().unwrap();
    guest.collaboration.room.as_mut().unwrap().capture_edit(&mut guest.session, edit).unwrap();
    assert!(guest.buffer_shared_typing(" later"));

    // The host's local deletion is confirmed before its event loop reads the guest's proposal.
    host.run("edit.clear", json!({"ids": [frame]})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "delete confirmed and stale text declined", |apps| {
        room(apps[0]).revision == 1 && room(apps[1]).revision == 1 && room(apps[1]).pending.is_none()
    });

    assert_eq!(document(&host), document(&guest));
    assert!(document(&guest).item(ItemId(frame)).is_none());
    assert!(document(&guest).story(designcraft_doc::StoryId(story)).is_none());
    assert!(guest.session.doc().unwrap().selection.text.is_none());
    assert_eq!(room(&guest).paused_typing(), Some(format!("{submitted} later").as_str()));
    assert!(guest.collaboration_blocks_close());
}

#[test]
fn shared_paragraph_formatting_preserves_concurrent_text_in_threaded_frames() {
    let mut host = editor(&Document::new(&NewDocument::default()), "Text host");
    host.run("style.paragraph.create", json!({"name": "Heading", "chars": {"size": 18}})).unwrap();
    let text = host.run("frame.create", json!({"rect": [40, 50, 300, 68], "content": "text", "text": "AB\nCD"})).unwrap();
    let first = text["id"].as_u64().unwrap();
    let story = text["story"].as_u64().unwrap();
    let story_id = StoryId(story);
    let frame = host.run("frame.create", json!({"rect": [40, 220, 300, 370], "content": "unassigned"})).unwrap();
    let second = frame["id"].as_u64().unwrap();
    host.run("text.thread", json!({"from": first, "to": second})).unwrap();
    let seed = document(&host).clone();
    let original = &seed.story(story_id).unwrap().paras[0];
    let mut guest = editor(&seed, "Text guest");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "text host starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "text guest joins", |apps| room(apps[1]).can_edit());

    // Both proposals start from the same revision, inside the second paragraph.
    host.run("text.select", json!({"story": story, "anchor": 4})).unwrap();
    host.run("type.alignCenter", json!({})).unwrap();
    guest.run("text.select", json!({"story": story, "anchor": 4})).unwrap();
    guest.run("text.insert", json!({"text": "λ", "raw": true})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "paragraph formatting and text converge", |apps| apps.iter().all(|app| room(app).revision == 2));

    guest.run("text.select", json!({"story": story, "anchor": 4})).unwrap();
    guest.run("type.para", json!({"attrs": {"leftIndent": 4.0, "spaceAfter": 7.0}})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "guest formats the same paragraph", |apps| apps.iter().all(|app| room(app).revision == 3));

    let assert_story = |host: &DesignApp, guest: &DesignApp, alignment| {
        assert_eq!(document(host), document(guest));
        let current = document(guest).story(story_id).unwrap();
        assert_eq!(current.frames, [ItemId(first), ItemId(second)]);
        assert_eq!(current.text, "AB\nCλD");
        assert_eq!(&current.paras[0], original);
        assert_eq!(current.paras[1].para.align, alignment);
        assert_eq!(current.paras[1].para.left_indent, Some(4.0));
        assert_eq!(current.paras[1].para.space_after, Some(7.0));
    };
    assert_story(&host, &guest, Some(Align::Center));
    for (command, revision, alignment) in [("edit.undo", 4, original.para.align), ("edit.redo", 5, Some(Align::Center))] {
        host.run(command, json!({})).unwrap();
        wait_for(&mut [&mut host, &mut guest], command, |apps| apps.iter().all(|app| room(app).revision == revision));
        assert_story(&host, &guest, alignment);
    }

    guest.run("text.select", json!({"story": story, "anchor": 0})).unwrap();
    guest.run("style.paragraph.apply", json!({"name": "Heading"})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "named style confirmed", |apps| apps.iter().all(|app| room(app).revision == 6));
    let styled = document(&guest).story(story_id).unwrap();
    assert_eq!(styled.paras[0].style, "Heading");
    assert_eq!(styled.paras[1].style, original.style);

    guest.run("text.select", json!({"story": story, "anchor": 2, "focus": 3})).unwrap();
    guest.run("text.insert", json!({"text": "\n", "raw": true})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "replaced break inherits the preceding style", |apps| apps.iter().all(|app| room(app).revision == 7));
    assert_eq!(document(&guest).story(story_id).unwrap().paras[1].style, "Heading");
    assert_eq!(document(&host), document(&guest));

    // Both writers start from the same revision. Undoing one field must preserve the other.
    host.run("text.select", json!({"story": story, "anchor": 0})).unwrap();
    guest.run("text.select", json!({"story": story, "anchor": 0})).unwrap();
    host.run("type.para", json!({"attrs": {"ragPattern": "stepped"}})).unwrap();
    guest.run("type.para", json!({"attrs": {"ragInset": 12.0}})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "rag edits converge", |apps| apps.iter().all(|app| room(app).revision == 9));
    for (command, revision, pattern) in [("edit.undo", 10, None), ("edit.redo", 11, Some(RagPattern::Stepped))] {
        assert_eq!(document(&host), document(&guest));
        host.run(command, json!({})).unwrap();
        wait_for(&mut [&mut host, &mut guest], command, |apps| apps.iter().all(|app| room(app).revision == revision));
        for app in [&host, &guest] {
            let attrs = &document(app).story(story_id).unwrap().paras[0].para;
            assert_eq!(attrs.rag_pattern, pattern);
            assert_eq!(attrs.rag_inset, Some(12.0));
        }
    }
    // Paragraph language applies to the same anchor as the pattern, but has its own history.
    let insets = [0.0, 18.0, 4.0, 27.0];
    host.run("type.para", json!({"attrs": {"ragPattern":"fourStop", "ragLineInsets":insets, "ragNearThreshold":8.5}})).unwrap();
    guest.run("type.paragraphLanguage", json!({"language":"French"})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "pattern and dictionary converge", |apps| apps.iter().all(|app| room(app).revision == 13));
    for (revision, command, language) in [(13, None, Some("French")), (14, Some("edit.undo"), None), (15, Some("edit.redo"), Some("French"))] {
        if let Some(command) = command {
            guest.run(command, json!({})).unwrap();
            wait_for(&mut [&mut host, &mut guest], command, |apps| apps.iter().all(|app| room(app).revision == revision));
        }
        assert_eq!(document(&host), document(&guest));
        let paragraphs = &document(&host).story(story_id).unwrap().paras;
        assert_eq!(paragraphs[0].chars.language.as_deref(), language);
        assert_eq!(paragraphs[0].para.rag_pattern, Some(RagPattern::FourStop));
        assert_eq!(paragraphs[0].para.rag_line_insets, Some(insets));
        assert_eq!(paragraphs[0].para.rag_near_threshold, Some(8.5));
        assert_eq!(paragraphs[1].chars.language, original.chars.language);
    }
    let confirmed = room(&host).history.iter().map(|(commit, _)| commit.clone()).collect();
    let recovered = super::replay::restore(&seed, &seed, confirmed).unwrap();
    assert_eq!(&recovered.document, document(&host));
}

#[test]
fn shared_text_undo_and_redo_preserve_the_other_writers_edits() {
    let mut host = editor(&Document::new(&NewDocument::default()), "Text host");
    let created = host.run("frame.create", json!({"rect": [40, 50, 300, 200], "content": "text", "text": "ABCD"})).unwrap();
    let story = created["story"].as_u64().unwrap();
    let story_id = StoryId(story);
    let seed = document(&host).clone();
    let mut guest = editor(&seed, "Text guest");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "text host starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "text guest joins", |apps| room(apps[1]).can_edit());

    let assert_story = |host: &DesignApp, guest: &DesignApp, expected| {
        assert_eq!(document(host), document(guest));
        assert_eq!(document(guest).story(story_id).unwrap().text, expected);
    };

    // Both proposals start at revision zero, at the same caret.
    for (app, text) in [(&mut host, "Ω"), (&mut guest, "éß")] {
        app.run("text.select", json!({"story": story, "anchor": 1})).unwrap();
        app.run("text.insert", json!({"text": text, "raw": true})).unwrap();
    }
    wait_for(&mut [&mut host, &mut guest], "both writers insert", |apps| apps.iter().all(|app| room(app).revision == 2));
    let mut with_guest = document(&host).story(story_id).unwrap().text.clone();
    assert!(with_guest.contains("éß") && with_guest.contains('Ω'));
    let between = with_guest.find('é').unwrap() + 'é'.len_utf8();
    with_guest.insert(between, '·');
    host.run("text.select", json!({"story": story, "anchor": between})).unwrap();
    host.run("text.insert", json!({"text": "·", "raw": true})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "host inserts inside guest text", |apps| apps.iter().all(|app| room(app).revision == 3));
    assert_story(&host, &guest, with_guest.as_str());
    let without_guest = with_guest.replace(['é', 'ß'], "");

    assert_eq!(guest.shared_history_enabled("edit.undo"), Some(true));
    for (command, revision, expected) in [("edit.undo", 4, without_guest.as_str()), ("edit.redo", 5, with_guest.as_str())] {
        guest.run(command, json!({})).unwrap();
        wait_for(&mut [&mut host, &mut guest], command, |apps| apps.iter().all(|app| room(app).revision == revision));
        assert_story(&host, &guest, expected);
    }

    // The guest removes seed atoms B and C while the host inserts between them.
    let start = with_guest.find('B').unwrap();
    guest.run("text.select", json!({"story": story, "anchor": start, "focus": start + 2})).unwrap();
    guest.run("text.delete", json!({})).unwrap();
    host.run("text.select", json!({"story": story, "anchor": start + 1})).unwrap();
    host.run("text.insert", json!({"text": "z", "raw": true})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "guest deletes around a host insertion", |apps| apps.iter().all(|app| room(app).revision == 7));
    let deleted = with_guest.replace("BCD", "zD");
    let restored = with_guest.replace("BCD", "BzCD");
    assert_story(&host, &guest, deleted.as_str());
    for (command, revision, expected) in [("edit.undo", 8, restored.as_str()), ("edit.redo", 9, deleted.as_str())] {
        guest.run(command, json!({})).unwrap();
        wait_for(&mut [&mut host, &mut guest], command, |apps| apps.iter().all(|app| room(app).revision == revision));
        assert_story(&host, &guest, expected);
    }

    let original_size = document(&guest).story(story_id).unwrap().char_format_of(0).over.size;
    guest.run("text.select", json!({"story": story, "anchor": 0, "focus": 1})).unwrap();
    guest.run("type.char", json!({"attrs": {"size": 24}})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "guest formats the first character", |apps| apps.iter().all(|app| room(app).revision == 10));
    host.run("text.select", json!({"story": story, "anchor": 0, "focus": 1})).unwrap();
    host.run("type.char", json!({"attrs": {"underline": true}})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "host underlines the same character", |apps| apps.iter().all(|app| room(app).revision == 11));
    for (command, revision, size) in [("edit.undo", 12, original_size), ("edit.redo", 13, Some(24.0))] {
        guest.run(command, json!({})).unwrap();
        wait_for(&mut [&mut host, &mut guest], command, |apps| apps.iter().all(|app| room(app).revision == revision));
        assert_story(&host, &guest, deleted.as_str());
        let format = document(&guest).story(story_id).unwrap().char_format_of(0);
        assert_eq!(format.over.size, size);
        assert_eq!(format.over.underline, Some(true));
    }

    let mut confirmed: Vec<_> = room(&host).history.iter().map(|(commit, _)| commit.clone()).collect();
    let recovered = super::replay::restore(&seed, &seed, confirmed.clone()).unwrap();
    assert_eq!(&recovered.document, document(&host));

    // Even the correct toggle cannot be sent directly as an ordinary edit.
    let first_undo = 3;
    let mut raw = confirmed[first_undo].proposal.clone();
    raw.base = room(&host).revision;
    raw.sequence = room(&guest).next_sequence;
    raw.history_action = None;
    deliver_room_packet(&guest, &mut host, super::Message::Propose(raw));
    assert_eq!(room(&host).revision, confirmed.last().unwrap().revision);
    assert_story(&host, &guest, deleted.as_str());

    // A correction cannot switch its target to the other author's actual batch.
    confirmed.truncate(first_undo + 1);
    let host_insert = &confirmed[first_undo - 1];
    confirmed[first_undo].proposal.edits = vec![Operation::SetTextBatchActive {
        story: story_id,
        actor: host_insert.actor.to_string(),
        original_sequence: host_insert.proposal.sequence,
        active: false,
    }];
    assert!(super::replay::restore(&seed, &seed, confirmed).is_err());

    host.run("layout.pages.insert", json!({"before": 0, "count": 1})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "host inserts a page", |apps| apps.iter().all(|app| room(app).revision == 14));
    guest.run("edit.undo", json!({})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "guest undoes formatting after repagination", |apps| apps.iter().all(|app| room(app).revision == 15));
    assert_story(&host, &guest, deleted.as_str());
    let format = document(&guest).story(story_id).unwrap().char_format_of(0);
    assert_eq!(format.over.size, original_size);
    assert_eq!(format.over.underline, Some(true));
}

#[test]
fn concurrent_editor_text_inserts_and_deletes_keep_the_other_peers_atoms() {
    let seed = Document::new(&NewDocument::default());
    let mut host = editor(&seed, "Text host");
    let created = host.run("frame.create", json!({"rect": [40, 50, 300, 200], "content": "text", "text": "ABCD"})).unwrap();
    let story = created["story"].as_u64().unwrap();
    let seed = document(&host).clone();
    let mut guest = editor(&seed, "Text guest");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "text host", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    join(&mut guest, &room(&host).invitation.clone().unwrap());
    wait_for(&mut [&mut host, &mut guest], "text guest", |apps| room(apps[1]).can_edit());

    // Both users type against revision zero at the same caret.
    for (app, text) in [(&mut host, "é"), (&mut guest, "Ω")] {
        app.run("text.select", json!({"story": story, "anchor": 1, "focus": 1})).unwrap();
        app.run("text.insert", json!({"text": text, "raw": true})).unwrap();
    }
    wait_for(&mut [&mut host, &mut guest], "concurrent multibyte insertion", |apps| apps.iter().all(|app| room(app).revision == 2));
    let story_id = designcraft_doc::StoryId(story);
    let text = document(&host).story(story_id).unwrap().text.clone();
    assert_eq!(document(&host), document(&guest));
    assert!(text.starts_with('A') && text.ends_with("BCD") && text.contains('é') && text.contains('Ω'));
    let guest_caret = guest.session.doc().unwrap().selection.text.unwrap().focus;
    assert!(text.is_char_boundary(guest_caret));
    assert!(text[..guest_caret].ends_with('Ω'));

    // One selection covers B and C while the other peer inserts between them.
    let start = text.find('B').unwrap();
    host.run("text.select", json!({"story": story, "anchor": start, "focus": start + 2})).unwrap();
    host.run("text.delete", json!({})).unwrap();
    guest.run("text.select", json!({"story": story, "anchor": start + 1, "focus": start + 1})).unwrap();
    guest.run("text.insert", json!({"text": "z", "raw": true})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "delete around concurrent insertion", |apps| apps.iter().all(|app| room(app).revision == 4));
    assert_eq!(document(&host), document(&guest));
    assert!(document(&host).story(story_id).unwrap().text.ends_with("zD"));

    // Disjoint override attributes can both survive on the same characters.
    for (app, attrs) in [(&mut host, json!({"size": 24})), (&mut guest, json!({"underline": true}))] {
        app.run("text.select", json!({"story": story, "anchor": 0, "focus": 1})).unwrap();
        app.run("type.char", json!({"attrs": attrs})).unwrap();
    }
    wait_for(&mut [&mut host, &mut guest], "concurrent character formatting", |apps| apps.iter().all(|app| room(app).revision == 6));
    assert_eq!(document(&host), document(&guest));
    let format = document(&guest).story(story_id).unwrap().char_format_of(0);
    assert_eq!(format.over.size, Some(24.0));
    assert_eq!(format.over.underline, Some(true));

    let end = document(&guest).story(story_id).unwrap().text.len();
    guest.run("text.select", json!({"story": story, "anchor": end, "focus": end})).unwrap();
    guest.run("text.insert", json!({"text": "1", "raw": true})).unwrap();
    assert!(guest.buffer_shared_typing("23"));
    wait_for(&mut [&mut host, &mut guest], "buffered typing", |apps| apps.iter().all(|app| room(app).revision == 8));
    assert_eq!(document(&host), document(&guest));
    assert!(document(&host).story(story_id).unwrap().text.ends_with("123"));

    // A fresh invitation from a restarted host keeps the guest's identity and its outstanding edit.
    guest.run("text.insert", json!({"text":"4","raw":true})).unwrap();
    let edit = guest.session.take_deferred_edit().unwrap();
    guest.collaboration.room.as_mut().unwrap().capture_edit(&mut guest.session, edit).unwrap();
    let guest_actor = room(&guest).local.as_ref().unwrap().actor;
    assert!(guest.collaboration_blocks_close());
    guest.request_quit();
    guest.handle_quit(&egui::Context::default());
    assert!(guest.collaboration.open, "quit directs the editor to its outstanding edit");
    assert!(room(&guest).pending.is_some());

    let state = host.collaboration.room.as_mut().unwrap();
    state.transport = super::Transport::host_with_identity("Text host", false, state.storage.identity()).unwrap();
    state.mode = super::Mode::Host { delivery: Default::default() };
    state.invitation = None;
    state.peers.clear();
    wait_for(&mut [&mut host], "rotated invitation", |apps| room(apps[0]).invitation.is_some());

    guest.collaboration.join_code = room(&host).invitation.clone().unwrap();
    guest.reconnect_collaboration().unwrap();
    wait_for(&mut [&mut host, &mut guest], "reconnect with a pending edit", |apps| apps.iter().all(|app| room(app).revision == 9));
    assert_eq!(room(&guest).local.as_ref().unwrap().actor, guest_actor);
    assert_eq!(document(&host), document(&guest));
    assert!(document(&host).story(story_id).unwrap().text.ends_with("1234"));

    let latest = room(&host).history.last().unwrap().0.clone();
    let unchanged = document(&guest).clone();
    let guest_room = guest.collaboration.room.as_mut().unwrap();
    guest_room.on_host_message(&mut guest.session, super::Message::Commit(latest.clone()));
    assert_eq!(guest_room.revision, 9);
    assert_eq!(*guest.session.doc().unwrap().doc, unchanged);

    let mut out_of_order = latest;
    out_of_order.revision += 2;
    guest_room.on_host_message(&mut guest.session, super::Message::Commit(out_of_order));
    assert!(!guest_room.can_edit());
    assert_eq!(guest_room.revision, 9);
    wait_for(&mut [&mut host, &mut guest], "catch up after a revision gap", |apps| room(apps[1]).can_edit());
    assert_eq!(*document(&guest), unchanged);
}

mod font_notice;
mod seed_artwork;

#[cfg(target_os = "macos")]
mod restart;

mod large_artwork;
