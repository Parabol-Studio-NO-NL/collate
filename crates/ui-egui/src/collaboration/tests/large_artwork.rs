//! End-to-end native placement, saved archive, and a new invitation from that archive.
use std::{fs::File, io, path::PathBuf};

use sha2::{Digest, Sha256};

use super::*;

#[test]
fn large_artwork_survives_guest_placement_save_and_a_new_host() {
    let files = tempfile::tempdir().unwrap();
    // Normal CI exercises the boundary above the old inline and seed limits. For a
    // capacity rehearsal, supply an artwork file and allow time for the full transfer.
    let (source, timeout) = if let Some(path) = std::env::var_os("COLLATE_TEST_ARTWORK") {
        (PathBuf::from(path), Duration::from_secs(900))
    } else {
        let path = files.path().join("artwork.png");
        std::fs::write(&path, artwork_fixture_with_side(431, 800)).unwrap();
        (path, Duration::from_secs(30))
    };
    let mut digest = Sha256::new();
    let length = io::copy(&mut File::open(&source).unwrap(), &mut digest).unwrap();
    assert!(length > 2 * 1024 * 1024);
    let expected = digest.finalize();
    let verify = |app: &DesignApp| {
        let asset = document(app).assets.values().next().unwrap();
        let bytes = asset.data.embedded_bytes().unwrap();
        assert_eq!(bytes.len() as u64, length);
        assert_eq!(Sha256::digest(bytes.as_slice()), expected);
    };

    let seed = Document::new(&NewDocument::default());
    let mut host = editor(&seed, "Studio");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "studio starts", |apps| room(apps[0]).can_edit());
    let mut guest = editor(&seed, "Designer");
    join(&mut guest, room(&host).invitation.as_deref().unwrap());
    wait_for(&mut [&mut host, &mut guest], "designer joins", |apps| room(apps[1]).can_edit());
    // Reject an oversized file before reading it. The same room can then place valid artwork.
    let oversized = files.path().join("oversized.png");
    File::create(&oversized).unwrap().set_len(designcraft_doc::MAX_SHARED_ARTWORK_BYTES as u64 + 1).unwrap();
    let error = guest.open_picked("place", oversized.to_str().unwrap().into()).unwrap_err();
    assert!(error.contains("1 GiB"));
    assert!(document(&guest).assets.is_empty() && room(&guest).pending.is_empty());
    guest.ui.dialog = None;
    guest.open_picked("place", source.to_str().unwrap().into()).unwrap();
    wait_for_with_deadline(&mut [&mut host, &mut guest], "placed artwork confirmed", timeout, |apps| {
        apps.iter().all(|app| room(app).can_edit() && room(app).revision == 1)
    });
    verify(&host);
    let path = files.path().join("Saved.designcraft");
    host.run("file.saveACopy", json!({"path": path})).unwrap();
    drop((host, guest));

    let mut reopened = DesignApp::new(Session::new(), Services::default());
    reopened.run("file.open", json!({"path": path})).unwrap();
    verify(&reopened);
    reopened.collaboration.name = "Studio".into();
    reopened.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut reopened], "saved studio starts", |apps| room(apps[0]).can_edit());
    let mut recipient = DesignApp::new(Session::new(), Services::default());
    recipient.collaboration.name = "Designer".into();
    recipient.collaboration.join_code = room(&reopened).invitation.clone().unwrap();
    recipient.start_seed_download().unwrap();
    wait_for_with_deadline(&mut [&mut reopened, &mut recipient], "new invitation delivers saved artwork", timeout, |apps| {
        apps[1].collaboration.room.as_ref().is_some_and(Room::can_edit)
    });
    verify(&recipient);
}
