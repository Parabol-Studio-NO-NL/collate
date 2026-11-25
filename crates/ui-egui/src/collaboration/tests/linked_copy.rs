use super::*;
use designcraft_doc::{AssetId, Storage};

#[test]
fn sharing_a_linked_book_keeps_the_original_document_and_opens_a_verified_room_copy() {
    let files = tempfile::tempdir().unwrap();
    let image = files.path().join("linked-illustration.png");
    let artwork = artwork_fixture_with_side(640, 400);
    std::fs::write(&image, &artwork).unwrap();
    let source = files.path().join("Book.designcraft");
    let copy = files.path().join("Shared Book.designcraft");

    let mut host = editor(&Document::new(&NewDocument::default()), "Book designer");
    let placed = host.run("file.place", json!({"path": image,"link":true})).unwrap();
    let asset_id = AssetId(placed["asset"].as_u64().unwrap());
    host.run("file.saveAs", json!({"path": source})).unwrap();
    let source_uid = host.session.doc().unwrap().uid;
    let saved_source = std::fs::read(&source).unwrap();

    host.start_linked_copy(copy.to_string_lossy().to_string()).unwrap();
    wait_for(&mut [&mut host], "the separate linked room copy starts", |apps| {
        apps[0].collaboration.room.as_ref().is_some_and(|room| room.can_edit() && room.invitation.is_some())
    });
    let hosted = host.session.doc().unwrap();
    assert_eq!(hosted.path.as_deref(), copy.to_str());
    assert_ne!(hosted.uid, source_uid);
    assert_eq!(hosted.doc.assets[&asset_id].storage, Storage::Embedded);
    let original = host.session.documents().iter().find(|doc| doc.uid == source_uid).unwrap();
    assert_eq!(original.path.as_deref(), source.to_str());
    assert_eq!(original.doc.assets[&asset_id].storage, Storage::Linked);
    assert_eq!(std::fs::read(source).unwrap(), saved_source);

    let mut guest = DesignApp::new(Session::new(), Services::default());
    guest.collaboration.name = "Proofreader".into();
    guest.collaboration.join_code = room(&host).invitation.clone().unwrap();
    guest.start_seed_download().unwrap();
    wait_for(&mut [&mut host, &mut guest], "the guest opens and verifies the copy's artwork", |apps| {
        apps[1].collaboration.room.as_ref().is_some_and(|room| room.can_edit() && !room.originals.is_pending())
    });
    let received = &document(&guest).assets[&asset_id];
    assert_eq!(received.storage, Storage::Embedded);
    assert!(received.link.is_none() && received.link_relative.is_none() && received.link_hash.is_none() && received.link_fingerprint.is_none());
    assert_eq!(received.output_bytes().unwrap().bytes.as_slice(), artwork.as_slice());
}
