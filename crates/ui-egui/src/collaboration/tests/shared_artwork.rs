use std::{path::Path, sync::Arc};

use designcraft_collab::artwork::BlobId;
use designcraft_doc::{AssetData, AssetId, StoryId};
use designcraft_engine::Session;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{artwork_fixture_with_side, document, editor, embedded_image_fixture, room, wait_for};
use crate::{DesignApp, Services, collaboration::shared_artwork::SharedArtworkFolder};

fn invited_guest(host: &DesignApp, name: &str, folder: &Path) -> DesignApp {
    let mut guest = DesignApp::new(Session::new(), Services::default());
    guest.collaboration.name = name.into();
    guest.collaboration.artwork_folder = Some(folder.to_path_buf());
    guest.collaboration.join_code = room(host).invitation.clone().unwrap();
    guest.start_seed_download().unwrap();
    guest
}

fn wait_without_host_originals(host: &mut DesignApp, guest: &mut DesignApp, stage: &str, done: impl Fn(&DesignApp) -> bool) {
    wait_for(&mut [guest], stage, |apps| {
        // Deliver the initial preview and confirmed edits without pumping the host's original-byte sender.
        let hosted = host.collaboration.room.as_mut().unwrap();
        for _ in 0..32 {
            let Some(event) = hosted.transport.try_event() else { break };
            hosted.on_event(&mut host.session, event);
        }
        assert!(!hosted.fatal, "{stage}: {:?}", hosted.error);
        hosted.advance_delivery();
        hosted.seed_delivery.advance(&hosted.transport, &hosted.storage);
        done(apps[0])
    });
}

fn assert_full_original(guest: &DesignApp, id: AssetId, expected: &[u8]) {
    let data = document(guest).assets[&id].output_bytes().unwrap();
    assert!(!data.proxied);
    assert!(data.bytes.as_slice() == expected, "the guest must receive the exact original bytes");
}

#[test]
fn a_configured_folder_hydrates_the_original_and_missing_files_use_the_peer() {
    let (mut seed, asset, _) = embedded_image_fixture();
    let bytes = artwork_fixture_with_side(871, 384);
    let image = Arc::make_mut(seed.assets.get_mut(&asset).unwrap());
    image.data = AssetData::embedded(bytes.clone());
    image.pixels = Some((384, 384));
    let blob = BlobId { digest: Sha256::digest(&bytes).into(), length: bytes.len().try_into().unwrap() };
    let folder = tempfile::tempdir().unwrap();
    SharedArtworkFolder::open(folder.path()).unwrap().publish(blob, &bytes).unwrap();

    let mut host = editor(&seed, "Book editor");
    let text = host.run("frame.create", json!({"rect": [140, 180, 330, 240], "content": "text", "text": "Proof"})).unwrap();
    let story = StoryId(text["story"].as_u64().unwrap());
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "the host advertises the illustrated document", |apps| room(apps[0]).invitation.is_some());

    let mut guest = invited_guest(&host, "Designer with synced artwork", folder.path());
    wait_without_host_originals(&mut host, &mut guest, "the first guest can edit", |guest| {
        guest.collaboration.room.as_ref().is_some_and(|room| room.can_edit())
    });
    guest.run("text.select", json!({"story": story.0, "anchor": "Proof".len()})).unwrap();
    guest.run("text.insert", json!({"text": " from the shared folder"})).unwrap();
    wait_without_host_originals(&mut host, &mut guest, "the folder supplies the original while the text edit syncs", |guest| {
        room(guest).revision == 1 && !room(guest).originals.is_pending()
    });
    assert_full_original(&guest, asset, &bytes);
    let edited = "Proof from the shared folder";
    assert_eq!(document(&host).story(story).unwrap().text, edited);

    let empty = tempfile::tempdir().unwrap();
    assert!(SharedArtworkFolder::open(empty.path()).unwrap().load(blob).unwrap().is_none());
    let mut late = invited_guest(&host, "Designer without synced artwork", empty.path());
    wait_without_host_originals(&mut host, &mut late, "the late guest catches up while its original is still held", |guest| {
        guest.collaboration.room.as_ref().is_some_and(|room| room.can_edit() && room.revision == 1)
    });
    assert_eq!(document(&late).story(story).unwrap().text, edited);
    assert!(document(&late).assets[&asset].data.shared_original_pending());

    wait_for(&mut [&mut host, &mut late], "the peer supplies the original absent from the folder", |apps| !room(apps[1]).originals.is_pending());
    assert_full_original(&late, asset, &bytes);
    assert_eq!(document(&late).story(story).unwrap().text, edited);
}
