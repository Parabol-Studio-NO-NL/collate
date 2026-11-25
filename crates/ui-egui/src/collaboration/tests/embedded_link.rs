use super::*;
use designcraft_doc::{Asset, AssetId, Storage};

fn has_local_provenance(asset: &Asset) -> bool {
    asset.link.is_some() || asset.link_relative.is_some() || asset.link_hash.is_some() || asset.link_fingerprint.is_some()
}

#[test]
fn embed_link_shares_the_original_bytes_and_keeps_the_source_file_private() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("art.png");
    let path = dir.path().join("layout.designcraft");
    let pixels = artwork_fixture(742);
    std::fs::write(&source, &pixels).unwrap();
    let mut host = editor(&Document::new(&NewDocument::default()), "Host");
    let placed = host.run("file.place", json!({"path": source, "link": true})).unwrap();
    let asset = AssetId(placed["asset"].as_u64().unwrap());
    assert_eq!(document(&host).assets[&asset].storage, Storage::Linked);
    assert!(host.connect_collaboration(true).is_err());
    assert_eq!(document(&host).assets[&asset].storage, Storage::Linked);

    host.run("links.embed", json!({"asset": asset.0})).unwrap();
    host.run("file.saveAs", json!({"path": path})).unwrap();
    let original = std::fs::read(&path).unwrap();
    let native = designcraft_format::load(&original).unwrap();
    let local = &native.assets[&asset];
    assert!(local.link.is_some() && local.link_relative.is_some() && local.link_hash.is_some() && local.link_fingerprint.is_some());
    assert!(super::super::seed_document::bundle(&native).is_err(), "an unsanitized room seed must still be rejected");
    std::fs::remove_file(&source).unwrap();

    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "embedded link host", |apps| room(apps[0]).invitation.is_some());
    let invite = room(&host).invitation.clone().unwrap();
    let mut guest = editor(&native, "Guest with original file");
    join(&mut guest, &invite);
    let mut invited = DesignApp::new(Session::new(), Services::default());
    invited.collaboration.name = "Guest from Home".into();
    invited.collaboration.join_code = invite;
    invited.start_seed_download().unwrap();
    wait_for(&mut [&mut host, &mut guest, &mut invited], "both guests join", |apps| {
        apps.iter().all(|app| app.collaboration.room.as_ref().is_some_and(Room::can_edit))
    });
    for app in [&host, &guest, &invited] {
        let received = &document(app).assets[&asset];
        assert_eq!(received.storage, Storage::Embedded);
        assert!(!has_local_provenance(received));
        assert!(received.data.embedded_bytes().unwrap().as_slice() == pixels);
    }
    assert!(std::fs::read(&path).unwrap() == original);

    #[cfg(unix)]
    {
        let server = dir.path().join("room");
        let export = dir.path().join("room.designcraft");
        super::super::server::init(&server, Some(&path)).unwrap();
        super::super::server::export(&server, &export).unwrap();
        let snapshot = designcraft_format::load(&std::fs::read(export).unwrap()).unwrap();
        assert!(!has_local_provenance(&snapshot.assets[&asset]));
        assert!(snapshot.assets[&asset].data.embedded_bytes().unwrap().as_slice() == pixels);
        assert!(std::fs::read(&path).unwrap() == original);
    }
}
