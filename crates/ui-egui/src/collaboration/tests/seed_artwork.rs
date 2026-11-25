use super::*;
use crate::collaboration::seed_document::{bundle, restore_bundle};
use designcraft_doc::{AssetData, AssetId};
use std::sync::Arc;

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
    host.collaboration.room.as_mut().unwrap().history[0].1.artwork_size = designcraft_doc::MAX_SHARED_ARTWORK_BYTES as u64 - 32;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inline.png");
    std::fs::write(&path, artwork_fixture(64)).unwrap();
    guest.open_picked("place", path.to_str().unwrap().into()).unwrap();
    wait_for(&mut [&mut host, &mut guest], "host reports artwork capacity", |apps| {
        room(apps[1]).error.as_deref() == Some(super::super::attachments::ARTWORK_LIMIT) && room(apps[1]).pending.is_none()
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
            1 => guest_room.clear_pending().unwrap(),
            2 => guest_room.pending.as_mut().unwrap().base = 0, // same sequence, different proposal metadata
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
