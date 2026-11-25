use std::{
    fs,
    net::{Ipv4Addr, UdpSocket},
    thread,
    time::{Duration, Instant},
};

use super::directory::{export, init, open, rotate_invite};
use super::*;
use crate::{DesignApp, Services};

fn wait_until(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for room");
        thread::sleep(Duration::from_millis(3));
    }
}

fn join(invite: &str) -> DesignApp {
    let mut guest = DesignApp::new(Session::new(), Services::default());
    guest.collaboration.name = "Studio guest".into();
    guest.collaboration.join_code = invite.into();
    guest.start_seed_download().unwrap();
    guest
}

fn settle(server: &mut Server, guest: &mut DesignApp, revision: u64) {
    let ctx = egui::Context::default();
    wait_until(|| {
        server.poll().unwrap();
        guest.poll_collaboration(&ctx);
        assert!(guest.collaboration.message.is_none(), "{:?}", guest.collaboration.message);
        guest.collaboration.room.as_ref().is_some_and(|room| room.can_edit() && room.revision == revision)
    });
}

fn artwork() -> Vec<u8> {
    use craft_image_codecs::{ChannelLayout, Format, Image};
    let mut random = 17_u32;
    let rgba = (0..768 * 768 * 4)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            random as u8
        })
        .collect();
    let image = Image::from_u8(768, 768, ChannelLayout::Rgba, rgba).unwrap();
    craft_image_codecs::encode(&image, Format::Png, &Default::default()).unwrap()
}

#[test]
fn unattended_room_keeps_confirmed_artwork_and_its_invitation_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("studio");
    let png = artwork();
    let file = temp.path().join("studio.png");
    fs::write(&file, &png).unwrap();
    init(&root, None).unwrap();
    let port = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap().local_addr().unwrap().port();
    let mut server = open(&root, false, port).unwrap();
    assert!(open(&root, false, 0).is_err());
    wait_until(|| {
        server.poll().unwrap();
        server.published.is_some()
    });
    let invite = fs::read_to_string(root.join("invitation.txt")).unwrap();
    let mut guest = join(&invite);
    settle(&mut server, &mut guest, 0);
    guest.open_picked("place", file.to_str().unwrap().into()).unwrap();
    settle(&mut server, &mut guest, 1);
    let old_doc = guest.session.doc().unwrap().doc.clone();
    let sidecars = fs::read_dir(root.join("state/blobs")).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(sidecars.len(), 1);
    assert_eq!(sidecars[0].metadata().unwrap().len(), png.len() as u64);
    drop(guest);
    drop(server);
    wait_until(|| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok());

    let mut server = open(&root, false, port).unwrap();
    let mut returning = join(&invite);
    settle(&mut server, &mut returning, 1);
    assert_eq!(returning.session.doc().unwrap().doc, old_doc);
    drop(returning);
    drop(server);

    let snapshot = temp.path().join("saved.designcraft");
    export(&root, &snapshot).unwrap();
    assert!(export(&root, &snapshot).is_err());
    let imported = temp.path().join("imported");
    init(&imported, Some(&snapshot)).unwrap();
    let reexported = temp.path().join("reimported.designcraft");
    export(&imported, &reexported).unwrap();
    let imported_seed = designcraft_format::load(&fs::read(reexported).unwrap()).unwrap();
    assert_eq!(imported_seed.assets.values().next().unwrap().data.embedded_bytes().unwrap().as_slice(), png);
    assert_eq!(fs::read_dir(imported.join("state/blobs")).unwrap().count(), 1);

    let header = || serde_json::from_slice::<Header>(&fs::read(root.join("state/room.json")).unwrap()).unwrap();
    let original = header();
    rotate_invite(&root).unwrap();
    let rotated = header();
    assert_eq!(original.identity, rotated.identity);
    assert_ne!(original.access, rotated.access);

    wait_until(|| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok());
    let mut rotated = open(&root, false, port).unwrap();
    wait_until(|| {
        rotated.poll().unwrap();
        rotated.published.is_some()
    });
    let (_, ticket) = RoomProtocol::parse_invitation(&invite);
    let mut refused = Transport::join(ticket, "Revoked invite").unwrap();
    wait_until(|| {
        rotated.poll().unwrap();
        while let Some(event) = refused.try_event() {
            match event {
                super::super::Event::Ready { .. } => panic!("revoked invitation joined the server"),
                super::super::Event::Failed(_) => return true,
                _ => {}
            }
        }
        false
    });
    drop(rotated);

    let mut corrupt = png;
    corrupt[0] ^= 1;
    fs::write(sidecars[0].path(), corrupt).unwrap();
    assert!(open(&root, false, port).err().unwrap().contains("content hash"));
}

#[cfg(feature = "automerge-rooms")]
#[test]
fn unattended_legacy_room_keeps_its_text_protocol_in_an_automerge_build() {
    use serde_json::json;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("legacy");
    init(&root, None).unwrap();
    let header_path = root.join("state/room.json");
    let seed_path = root.join("state/seed");
    let mut header: Header = serde_json::from_slice(&fs::read(&header_path).unwrap()).unwrap();
    let seed = seed_document::restore_bundle_for_room(
        &fs::read(&seed_path).unwrap(),
        |_| Err("Empty fixture has no artwork.".into()),
        RoomProtocol::Automerge,
    )
    .unwrap();
    let (seed_bytes, blobs) = seed_document::bundle_for_room(&seed, RoomProtocol::Legacy).unwrap();
    assert!(blobs.is_empty());
    header.version = 1;
    header.seed = seed_document::Fingerprints::for_room(RoomProtocol::Legacy).fingerprint(&seed).unwrap();
    fs::write(header_path, serde_json::to_vec(&header).unwrap()).unwrap();
    fs::write(seed_path, seed_bytes).unwrap();

    rotate_invite(&root).unwrap();
    let mut server = open(&root, false, 0).unwrap();
    wait_until(|| {
        server.poll().unwrap();
        server.published.is_some()
    });
    let invite = fs::read_to_string(root.join("invitation.txt")).unwrap();
    assert_eq!(RoomProtocol::parse_invitation(&invite).0, RoomProtocol::Legacy);
    let mut guest = join(&invite);
    settle(&mut server, &mut guest, 0);
    let start = "Épreuve";
    let frame = guest.run("frame.create", json!({"rect": [30, 40, 280, 200], "content": "text", "text": start})).unwrap();
    settle(&mut server, &mut guest, 1);
    guest.run("text.select", json!({"story": frame["story"], "anchor": start.len(), "focus": start.len()})).unwrap();
    guest.run("text.insert", json!({"text": " corrigée", "raw": true})).unwrap();
    settle(&mut server, &mut guest, 2);
    let inserted = &server.room.history.last().unwrap().0.proposal;
    assert!(inserted.story_transaction.is_none());
    assert!(matches!(inserted.edits.as_slice(), [designcraft_collab::Operation::Text { .. }]));
    drop(guest);
    drop(server);

    let output = temp.path().join("proof.designcraft");
    export(&root, &output).unwrap();
    let snapshot = designcraft_format::load(&fs::read(output).unwrap()).unwrap();
    assert!(snapshot.stories.values().any(|story| story.text == "Épreuve corrigée"));
}
