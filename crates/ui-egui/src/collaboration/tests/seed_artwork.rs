use super::*;
use crate::collaboration::seed_document::{bundle, restore_bundle};
use designcraft_doc::{AssetData, AssetId};
use std::sync::Arc;

#[cfg(feature = "automerge-rooms")]
#[test]
fn a_preview_seed_keeps_the_original_fingerprint_with_multiple_native_proxies() {
    use crate::collaboration::seed_document::{preview_seed, restore_preview_seed};

    let (mut seed, id, _) = embedded_image_fixture();
    let asset = Arc::make_mut(seed.assets.get_mut(&id).unwrap());
    asset.data = AssetData::embedded(artwork_fixture_with_side(120, 180));
    asset.pixels = Some((180, 180));
    let mut another = asset.clone();
    // "100" sorts before the first asset's name, but its numeric ID follows it.
    another.id = AssetId(100);
    another.data = AssetData::embedded(artwork_fixture_with_side(322, 180));
    seed.next_id = seed.next_id.max(another.id.0);
    seed.assets.insert(another.id, Arc::new(another));

    let mut preview = preview_seed(&seed).unwrap();
    let (partial, originals) = restore_preview_seed(&preview.archive, |id| Ok(preview.eager.remove(&id).unwrap())).unwrap();
    assert!(preview.eager.is_empty());
    assert_eq!(originals.len(), seed.assets.len());
    assert_eq!(fingerprint(&partial).unwrap(), fingerprint(&seed).unwrap());
    for (blob, targets) in originals {
        for target in targets {
            assert!(target.data.install_verified_original(preview.originals[&blob].clone()));
        }
    }
    assert_eq!(partial, seed);
    assert_eq!(fingerprint(&partial).unwrap(), fingerprint(&seed).unwrap());
}

#[cfg(feature = "automerge-rooms")]
#[test]
fn many_individually_small_originals_open_as_bounded_raster_previews() {
    use crate::collaboration::seed_document::{preview_seed, restore_preview_seed, use_preview_seed, verify_original};

    let (mut seed, icon, _) = embedded_image_fixture();
    let side = 72;
    // Reuse the synthetic illustration generator from the book fixture.
    for number in 1..=80 {
        let id = AssetId(seed.alloc());
        let asset = designcraft_doc::Asset {
            id,
            name: format!("Book illustration {number}.png"),
            mime: "image/png".into(),
            data: AssetData::embedded(artwork_fixture_with_side(number, side)),
            pixels: Some((side, side)),
            ..Default::default()
        };
        seed.assets.insert(id, Arc::new(asset));
    }

    assert!(use_preview_seed(&seed));
    let mut preview = preview_seed(&seed).unwrap();
    let original_bytes: u64 = seed.assets.values().map(|asset| asset.data.len()).sum();
    let eager_bytes: usize = preview.eager.values().map(|bytes| bytes.len()).sum();
    assert!((eager_bytes as u64) < original_bytes / 2);
    let (partial, originals) = restore_preview_seed(&preview.archive, |id| Ok(preview.eager.remove(&id).unwrap())).unwrap();
    assert_eq!(partial.assets[&icon], seed.assets[&icon]);
    assert_eq!(originals.len(), seed.assets.len() - 1);
    assert_eq!(fingerprint(&partial).unwrap(), fingerprint(&seed).unwrap());

    for (id, targets) in originals {
        let bytes = &preview.originals[&id];
        for target in targets {
            verify_original(&target, bytes).unwrap();
            assert!(target.data.install_verified_original(bytes.clone()));
        }
    }
    assert_eq!(partial, seed);
}

#[cfg(feature = "automerge-rooms")]
#[test]
fn a_guest_receives_the_full_seed_when_the_preview_manifest_exceeds_the_model_limit() {
    use crate::collaboration::seed_document::{MAX_MODEL_BYTES, model_bytes, preview_seed, use_preview_seed};
    use designcraft_doc::{Content, Graphic, SpreadRef};

    let (mut seed, asset, frame) = embedded_image_fixture();
    let asset_template = (*seed.assets[&asset]).clone();
    let frame_template = seed.item(frame).unwrap().clone();
    let graphic_template = frame_template.graphic().unwrap();
    let side = 120;
    for number in 1..=10 {
        let id = AssetId(seed.alloc());
        let asset = designcraft_doc::Asset {
            id,
            data: AssetData::embedded(artwork_fixture_with_side(number, side)),
            pixels: Some((side, side)),
            proxy: None,
            ..asset_template.clone()
        };
        seed.assets.insert(id, Arc::new(asset));
        let mut placed = frame_template.clone();
        placed.id = designcraft_doc::ItemId(seed.alloc());
        placed.content = Content::Graphic(Graphic { asset: id, ..graphic_template.clone() });
        seed.insert_item(SpreadRef::Doc(0), placed, None).unwrap();
    }
    // A compressible field fills the model without requiring a long story to compose.
    seed.file_info.description.push('x');
    let remaining = MAX_MODEL_BYTES - model_bytes(&seed).unwrap().len();
    seed.file_info.description.push_str(&"x".repeat(remaining - 256));
    assert!(use_preview_seed(&seed));
    assert!(preview_seed(&seed).is_err());

    let mut host = editor(&seed, "Book host");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "host invites guests to the full seed", |apps| room(apps[0]).invitation.is_some());
    let mut guest = DesignApp::new(Session::new(), Services::default());
    guest.collaboration.name = "Book guest".into();
    guest.collaboration.join_code = room(&host).invitation.clone().unwrap();
    guest.start_seed_download().unwrap();
    wait_for(&mut [&mut host, &mut guest], "guest receives the full seed", |apps| apps[1].collaboration.room.as_ref().is_some_and(Room::can_edit));
    assert!(!room(&guest).originals.is_pending());
    assert_eq!(fingerprint(document(&guest)).unwrap(), fingerprint(&seed).unwrap());
}

#[cfg(feature = "automerge-rooms")]
#[test]
fn a_guest_edits_from_previews_before_the_verified_original_is_available() {
    let (mut seed, asset, _) = embedded_image_fixture();
    let bytes = artwork_fixture_with_side(781, 720);
    assert!(bytes.len() > designcraft_collab_core::MAX_PAYLOAD);
    let artwork = Arc::make_mut(seed.assets.get_mut(&asset).unwrap());
    artwork.data = AssetData::embedded(bytes.clone());
    artwork.pixels = Some((720, 720));

    let mut host = editor(&seed, "Illustrated book host");
    let created = host.run("frame.create", json!({"rect": [140, 180, 330, 240], "content": "text", "text": "Proof"})).unwrap();
    let story = StoryId(created["story"].as_u64().unwrap());
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "host advertises the illustrated book", |apps| room(apps[0]).invitation.is_some());
    let mut guest = DesignApp::new(Session::new(), Services::default());
    guest.collaboration.name = "Remote designer".into();
    guest.collaboration.join_code = room(&host).invitation.clone().unwrap();
    guest.start_seed_download().unwrap();
    wait_for(&mut [&mut host, &mut guest], "guest can edit the preview document", |apps| {
        apps[1].collaboration.room.as_ref().is_some_and(Room::can_edit)
    });
    let asset = *document(&guest).assets.keys().next().unwrap();
    let downloaded = &document(&guest).assets[&asset];
    assert!(downloaded.data.shared_original_pending());
    assert!(downloaded.data.shared_preview().is_some_and(|preview| !preview.data.is_empty()));
    assert_eq!(downloaded.output_bytes().err(), Some(designcraft_doc::Unavailable::SharedOriginalPending));
    assert!(designcraft_format::save(document(&guest)).is_err());
    assert!(guest.run("file.exportPdf", json!({})).unwrap_err().to_string().contains("original artwork"));

    guest.run("text.select", json!({"story": story.0, "anchor": 5, "focus": 5})).unwrap();
    guest.run("text.insert", json!({"text": " edited before downloading"})).unwrap();
    let wait_without_originals = |host: &mut DesignApp, guest: &mut DesignApp, revision| {
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(30);
        // Pump the host's edit channel while deliberately holding its original queue.
        while room(guest).revision < revision {
            guest.poll_collaboration(&ctx);
            let hosted = host.collaboration.room.as_mut().unwrap();
            while let Some(event) = hosted.transport.try_event() {
                hosted.on_event(&mut host.session, event);
            }
            hosted.advance_delivery();
            assert!(Instant::now() < deadline, "the edit should reach the host before the original transfers");
            thread::sleep(Duration::from_millis(2));
        }
    };
    wait_without_originals(&mut host, &mut guest, 1);
    assert_eq!(document(&host).story(story).unwrap().text, "Proof edited before downloading");
    assert!(document(&guest).assets[&asset].data.shared_original_pending());

    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("the-same-image.png");
    std::fs::write(&path, &bytes).unwrap();
    guest.run("selection.set", json!({"ids": []})).unwrap();
    guest.open_picked("place", path.to_str().unwrap().into()).unwrap();
    wait_without_originals(&mut host, &mut guest, 2);
    assert!(document(&guest).assets[&asset].data.shared_original_pending());
    assert_eq!(document(&host).assets.len(), 1);
    assert_eq!(document(&guest).assets.len(), 1);
    assert_eq!(document(&guest).placed_items().iter().filter(|frame| frame.graphic().is_some_and(|graphic| graphic.asset == asset)).count(), 2);

    wait_for(&mut [&mut host, &mut guest], "guest verifies the original artwork", |apps| !room(apps[1]).originals.is_pending());
    let hydrated = &document(&guest).assets[&asset];
    let output = hydrated.output_bytes().unwrap();
    assert!(!output.proxied);
    assert_eq!(output.bytes.as_slice(), bytes.as_slice());
    assert_eq!(fingerprint(document(&host)).unwrap(), fingerprint(document(&guest)).unwrap());
    assert!(designcraft_format::save(document(&guest)).is_ok());
}

#[test]
fn large_seed_reuses_shared_bodies_and_checks_aggregate_length_before_loading() {
    let (mut seed, id, _) = embedded_image_fixture();
    let body = Arc::new(artwork_fixture_with_side(741, 832));
    let duplicate = AssetId(seed.alloc());
    let asset = Arc::make_mut(seed.assets.get_mut(&id).unwrap());
    asset.data = AssetData::embedded(body);
    asset.pixels = Some((832, 832));
    let mut copy = asset.clone();
    copy.id = duplicate;
    seed.assets.insert(duplicate, Arc::new(copy));

    let (bytes, mut blobs) = bundle(&seed).unwrap();
    assert!(bytes.len() < designcraft_collab_core::MAX_PAYLOAD);
    let received = restore_bundle(&bytes, |id| Ok(blobs.remove(&id).unwrap())).unwrap();
    assert!(blobs.is_empty());
    assert_eq!(received, seed);
    assert!(Arc::ptr_eq(received.assets[&id].data.embedded_bytes().unwrap(), received.assets[&duplicate].data.embedded_bytes().unwrap()));

    let prefix = bytes.iter().position(|&b| b == b'\n').unwrap() + 1;
    let mut manifest: serde_json::Value = serde_json::from_slice(&bytes[prefix..]).unwrap();
    for entry in manifest["members"].as_object_mut().unwrap().values_mut() {
        entry[0]["length"] = designcraft_collab::artwork::MAX_ARTWORK_BYTES.into();
    }
    let mut oversized = bytes[..prefix].to_vec();
    serde_json::to_writer(&mut oversized, &manifest).unwrap();
    assert!(restore_bundle(&oversized, |_| panic!("over-limit artwork read from disk")).is_err());
}

#[test]
fn guest_can_edit_after_the_host_rejects_artwork_at_its_retained_limit() {
    let seed = Document::new(&NewDocument::default());
    let mut host = editor(&seed, "Host");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "host starts", |apps| room(apps[0]).invitation.is_some());
    let mut guest = editor(&seed, "Guest");
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "guest joins", |apps| room(apps[1]).can_edit());
    host.run("frame.create", json!({"rect": [10, 10, 80, 80]})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "first confirmed change", |apps| apps.iter().all(|a| room(a).revision == 1));

    // Simulate other artwork having consumed the host's quota while the guest
    // was preparing a proposal, without allocating a gigabyte in regular CI.
    host.collaboration.room.as_mut().unwrap().history_artwork = designcraft_doc::MAX_SHARED_ARTWORK_BYTES as u64 - 32;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("small.png");
    std::fs::write(&path, artwork_fixture(64)).unwrap();
    guest.open_picked("place", path.to_str().unwrap().into()).unwrap();
    wait_for(&mut [&mut host, &mut guest], "host reports artwork capacity", |apps| {
        room(apps[1]).error.as_deref() == Some(super::super::attachments::ARTWORK_LIMIT) && room(apps[1]).pending.is_empty()
    });
    assert!(document(&host).assets.is_empty() && document(&guest).assets.is_empty());
    guest.run("frame.create", json!({"rect": [110, 10, 180, 80]})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "ordinary edit after rejection", |apps| apps.iter().all(|a| room(a).revision == 2));
    assert_eq!(document(&host), document(&guest));
}

#[test]
fn author_receives_a_bound_receipt_and_can_recover_without_pending_artwork() {
    let seed = Document::new(&NewDocument::default());
    let mut host = editor(&seed, "Host");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "host starts", |apps| room(apps[0]).invitation.is_some());
    let mut guest = editor(&seed, "Designer");
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "guest joins", |apps| room(apps[1]).can_edit());

    let files = tempfile::tempdir().unwrap();
    let ctx = egui::Context::default();
    for index in 0..3 {
        let revision = index as u64 + 1;
        let body = artwork_fixture_with_side(171 + index as u32, 800);
        let path = files.path().join(format!("art-{index}.png"));
        std::fs::write(&path, &body).unwrap();
        guest.open_picked("place", path.to_str().unwrap().into()).unwrap();
        guest.poll_collaboration(&ctx);
        let deadline = Instant::now() + Duration::from_secs(30);
        // Keep the author's incoming messages queued until the host accepts the upload.
        while room(&host).revision < revision {
            guest.collaboration.room.as_mut().unwrap().advance_proposal();
            host.poll_collaboration(&ctx);
            assert!(Instant::now() < deadline, "upload stalled");
            thread::sleep(Duration::from_millis(2));
        }
        let guest_room = guest.collaboration.room.as_mut().unwrap();
        match index {
            1 => guest_room.clear_pending(&mut guest.session).unwrap(),
            2 => guest_room.pending.front_mut().unwrap().base = 0, // same sequence, different proposal metadata
            _ => {}
        }
        let mut received_binary = 0;
        let mut receipts = 0;
        while guest_room.revision < revision {
            host.poll_collaboration(&ctx);
            while let Some(event) = guest_room.transport.try_event() {
                if let super::super::Event::Message { bytes, .. } = &event {
                    if super::super::attachments::is_part(bytes) {
                        received_binary += bytes.len();
                    } else if serde_json::from_slice::<super::super::Packet>(bytes)
                        .is_ok_and(|p| matches!(p.message, super::super::Message::ArtworkReceipt { .. }))
                    {
                        receipts += 1;
                    }
                }
                guest_room.on_event(&mut guest.session, event);
            }
            guest_room.advance_delivery();
            assert!(!guest_room.fatal, "{:?}", guest_room.error);
            assert!(Instant::now() < deadline, "confirmation stalled");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(receipts, 1);
        if index != 0 {
            assert!(received_binary >= body.len(), "fresh replay supplies the complete unconfirmed body");
        } else {
            assert_eq!(received_binary, 0, "submitting guest needs only its authenticated receipt");
        }
        assert_eq!(document(&guest), document(&host));
        wait_for(&mut [&mut host, &mut guest], "guest caught up", |apps| room(apps[1]).can_edit());
    }
}

#[test]
fn smaller_placements_stay_out_of_json_history_and_inline_journals_still_replay() {
    use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};

    let seed = Document::new(&NewDocument::default());
    let mut host = editor(&seed, "Host");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "host starts", |apps| room(apps[0]).invitation.is_some());
    let mut guest = editor(&seed, "Guest");
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "guest joins", |apps| room(apps[1]).can_edit());

    let dir = tempfile::tempdir().unwrap();
    let mut old_size = 0;
    for index in 0..4 {
        let body = artwork_fixture_with_side(371 + index, 700);
        assert!(body.len() < 2 * 1024 * 1024);
        old_size += (body.len() * 4).div_ceil(3);
        let path = dir.path().join(format!("spread-{index}.png"));
        std::fs::write(&path, body).unwrap();
        host.open_picked("place", path.to_str().unwrap().into()).unwrap();
        wait_for(&mut [&mut host, &mut guest], "placed image confirmed", |apps| apps.iter().all(|app| room(app).revision == u64::from(index) + 1));
    }
    assert!(old_size > 8 * 1024 * 1024);
    assert!(room(&host).history_bytes < designcraft_collab_core::MAX_PAYLOAD);
    assert_eq!(document(&host), document(&guest));

    // Produce a v13 journal entry: it held base64 bytes instead of a body length.
    let (first, wire) = &room(&host).history[0];
    let original = wire.blobs.values().next().unwrap();
    let mut record = serde_json::to_value(first).unwrap();
    let artwork = record["proposal"]["edits"][0]["placement"]["artwork"].as_object_mut().unwrap();
    assert!(artwork.remove("length").is_some());
    artwork.insert("bytes".into(), STANDARD_NO_PAD.encode(original.as_slice()).into());
    let legacy: super::super::Commit = serde_json::from_value(record.clone()).unwrap();
    let restored = super::super::replay::restore(&seed, &seed, vec![legacy]).unwrap();
    let installed = restored.document.assets.values().next().unwrap().data.embedded_bytes().unwrap();
    assert_eq!(installed, original);

    let mut corrupt = original.as_ref().clone();
    *corrupt.last_mut().unwrap() ^= 1;
    record["proposal"]["edits"][0]["placement"]["artwork"]["bytes"] = STANDARD_NO_PAD.encode(corrupt).into();
    assert!(super::super::replay::restore(&seed, &seed, vec![serde_json::from_value(record).unwrap()]).is_err());
}
