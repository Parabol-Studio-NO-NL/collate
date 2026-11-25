use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use designcraft_collab_core::{Identity, journal::with_test_keychain};
use security_framework::os::macos::keychain::{CreateOptions, SecKeychain};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::*;

const CHILD_ROOT: &str = "COLLATE_TEST_ROOM_RESTART_ROOT";
const COMMENT: &str = "Check the recovered cover frame.";

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    host_actor: String,
    guest_actor: String,
    invitation_digest: [u8; 32],
    document: [u8; 32],
    frame: SharedId,
    thread: SharedId,
    frame_x: f64,
    pending_sequence: u64,
}

struct DisposableKeychain(PathBuf);

impl Drop for DisposableKeychain {
    fn drop(&mut self) {
        if !Command::new("/usr/bin/security").arg("delete-keychain").arg(&self.0).output().is_ok_and(|result| result.status.success()) {
            eprintln!("Could not delete the disposable Collate room test keychain.");
        }
    }
}

fn reopened(root: &Path, role: &str) -> DesignApp {
    let mut session = Session::new();
    session.recovery_dir = Some(root.join(role).join("Recovery"));
    let mut app = DesignApp::new(session, Services::default());
    app.collaboration.name = role.into();
    app.run("file.open", json!({"path": root.join(format!("{role}.designcraft"))})).unwrap();
    app
}

fn actor(app: &DesignApp) -> String {
    room(app).local.as_ref().unwrap().actor.to_string()
}

fn assert_comment(app: &DesignApp, checkpoint: &Checkpoint) {
    let mut comments = room(app).replica.comments();
    let comment = comments.next().unwrap();
    assert_eq!(comment.id, checkpoint.thread);
    assert_eq!(comment.anchor, CommentAnchor::Object(checkpoint.frame.clone()));
    assert_eq!(comment.text, COMMENT);
    assert!(comment.resolved);
    assert!(comments.next().is_none());
}

// A real process restart must also restore encrypted artwork outside the bounded
// JSON record, both from the seed and from a later placement.
fn save_artwork_checkpoint(root: &Path) {
    let (mut doc, id, _) = embedded_image_fixture();
    let pixels = artwork_fixture_with_side(589, 832);
    let asset = std::sync::Arc::make_mut(doc.assets.get_mut(&id).unwrap());
    asset.data = designcraft_doc::AssetData::embedded(pixels);
    asset.pixels = Some((832, 832));
    let mut source = editor(&doc, "Studio artwork");
    let original = root.join("formerly-linked.png");
    std::fs::write(&original, artwork_fixture(555)).unwrap();
    let placed = source.run("file.place", json!({"path": original, "link": true})).unwrap();
    source.run("links.embed", json!({"asset": placed["asset"]})).unwrap();
    source.run("file.saveAs", json!({"path": root.join("Artwork.designcraft")})).unwrap();
    drop(source);
    std::fs::remove_file(original).unwrap();
    let mut host = reopened(root, "Artwork");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "durable artwork host", |apps| room(apps[0]).can_edit());
    let artwork = root.join("placed.png");
    std::fs::write(&artwork, artwork_fixture_with_side(3775, 1032)).unwrap();
    host.open_picked("place", artwork.to_str().unwrap().into()).unwrap();
    wait_for(&mut [&mut host], "journal a multi-chunk placement", |apps| room(apps[0]).revision == 1);
}

fn restore_artwork_checkpoint(root: &Path) {
    let mut host = reopened(root, "Artwork");
    host.connect_collaboration(true).unwrap();
    assert_eq!(room(&host).revision, 1);
    let doc = document(&host);
    let placed = doc.assets.values().find(|asset| asset.name == "placed.png").unwrap();
    assert_eq!(placed.data.embedded_bytes().unwrap().as_slice(), std::fs::read(root.join("placed.png")).unwrap());
    let seed = doc.assets.values().find(|asset| asset.name == "Synthetic colour chip").unwrap();
    assert_eq!(seed.data.embedded_bytes().unwrap().as_slice(), artwork_fixture_with_side(589, 832));
    let embedded = doc.assets.values().find(|asset| asset.name == "formerly-linked.png").unwrap();
    assert!(embedded.link.is_none() && embedded.link_relative.is_none() && embedded.link_hash.is_none() && embedded.link_fingerprint.is_none());
    assert!(embedded.data.embedded_bytes().unwrap().as_slice() == artwork_fixture(555));
}

/// Stage exactly one outbox entry, without polling the host or delivering the guest's edit.
fn save_checkpoint(root: &Path) {
    save_artwork_checkpoint(root);
    let seed = Document::new(&NewDocument::default());
    let mut initial = editor(&seed, "Fixture");
    let host_path = root.join("Host.designcraft");
    initial.run("file.saveAs", json!({"path": host_path})).unwrap();
    std::fs::copy(&host_path, root.join("Guest.designcraft")).unwrap();
    drop(initial);

    let mut host = reopened(root, "Host");
    let mut guest = reopened(root, "Guest");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "durable host starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    let invitation = room(&host).invitation.clone().unwrap();
    join(&mut guest, &invitation);
    wait_for(&mut [&mut host, &mut guest], "durable guest joins", |apps| room(apps[1]).can_edit());
    assert!(room(&host).storage.is_durable() && room(&guest).storage.is_durable());

    let created = host.run("frame.create", json!({"rect": [30, 40, 160, 200], "content": "unassigned"})).unwrap();
    let id = ItemId(created["id"].as_u64().unwrap());
    wait_for(&mut [&mut host, &mut guest], "confirm frame", |apps| apps.iter().all(|app| room(app).revision == 1));
    let frame = room(&host).replica.shared_id(id).unwrap();
    let guest_room = guest.collaboration.room.as_mut().unwrap();
    let thread = SharedId::Created { actor: guest_room.local.as_ref().unwrap().actor.to_string(), sequence: guest_room.next_sequence, index: 0 };
    guest_room
        .queue_local(
            &mut guest.session,
            vec![Operation::CreateComment { id: thread.clone(), anchor: CommentAnchor::Object(frame.clone()), text: COMMENT.into() }],
        )
        .unwrap();
    wait_for(&mut [&mut host, &mut guest], "confirm comment", |apps| apps.iter().all(|app| room(app).revision == 2));
    host.collaboration
        .room
        .as_mut()
        .unwrap()
        .queue_local(&mut host.session, vec![Operation::SetResolved { thread: thread.clone(), resolved: true }])
        .unwrap();
    wait_for(&mut [&mut host, &mut guest], "confirm resolution", |apps| apps.iter().all(|app| room(app).revision == 3));

    host.run("file.save", json!({})).unwrap();
    let guest_id = room(&guest).replica.local_id(&frame).unwrap();
    guest.run("transform.move", json!({"ids": [guest_id.0], "dx": 11, "dy": 0})).unwrap();
    let edit = guest.session.take_deferred_edit().unwrap();
    let guest_room = guest.collaboration.room.as_mut().unwrap();
    guest_room.capture_edit(&mut guest.session, edit).unwrap();
    let pending_sequence = guest_room.pending.as_ref().unwrap().sequence;
    assert_eq!(room(&host).revision, 3);
    let checkpoint = Checkpoint {
        host_actor: actor(&host),
        guest_actor: actor(&guest),
        invitation_digest: Sha256::digest(invitation.as_bytes()).into(),
        document: fingerprint(document(&host)).unwrap(),
        frame,
        thread,
        frame_x: document(&host).item(id).unwrap().bounds().x0,
        pending_sequence,
    };
    std::fs::write(root.join("checkpoint.json"), serde_json::to_vec(&checkpoint).unwrap()).unwrap();

    for role in ["Host", "Guest"] {
        let journal = root.join(role).join("Collaboration");
        let records: Vec<_> = std::fs::read_dir(journal)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "enc"))
            .collect();
        assert_eq!(records.len(), 1);
        let path = &records[0];
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.starts_with(b"CDJRNL01"));
        assert!(!bytes.windows(COMMENT.len()).any(|part| part == COMMENT.as_bytes()));
    }
}

fn read_checkpoint(root: &Path) -> Checkpoint {
    serde_json::from_slice(&std::fs::read(root.join("checkpoint.json")).unwrap()).unwrap()
}

fn restore_checkpoint(root: &Path) {
    restore_artwork_checkpoint(root);
    let checkpoint = read_checkpoint(root);
    let mut host = reopened(root, "Host");
    assert_eq!(fingerprint(document(&host)).unwrap(), checkpoint.document);
    host.connect_collaboration(true).unwrap();
    assert_eq!(room(&host).revision, 3);
    assert_comment(&host, &checkpoint);
    wait_for(&mut [&mut host], "restored host advertises a fresh invitation", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    assert_eq!(actor(&host), checkpoint.host_actor);
    let invitation = room(&host).invitation.clone().unwrap();
    let invitation_digest: [u8; 32] = Sha256::digest(invitation.as_bytes()).into();
    assert!(invitation_digest != checkpoint.invitation_digest);

    let mut guest = reopened(root, "Guest");
    // The guest's file is still the seed. The journal must reconstruct the confirmed layout.
    assert_ne!(fingerprint(document(&guest)).unwrap(), checkpoint.document);
    join(&mut guest, &invitation);
    assert_eq!(fingerprint(document(&guest)).unwrap(), checkpoint.document);
    assert_eq!(room(&guest).pending.as_ref().unwrap().sequence, checkpoint.pending_sequence);
    assert_comment(&guest, &checkpoint);
    wait_for(&mut [&mut host, &mut guest], "replay saved outbox exactly once", |apps| {
        apps.iter().all(|app| room(app).revision == 4 && room(app).pending.is_none())
    });
    assert_eq!(actor(&guest), checkpoint.guest_actor);
    let host_id = room(&host).replica.local_id(&checkpoint.frame).unwrap();
    let guest_id = room(&guest).replica.local_id(&checkpoint.frame).unwrap();
    assert_eq!(document(&host).item(host_id).unwrap().bounds().x0, checkpoint.frame_x + 11.0);

    // A fresh edit must use the next sequence after the restored outbox.
    guest.run("transform.move", json!({"ids": [guest_id.0], "dx": 7, "dy": 0})).unwrap();
    wait_for(&mut [&mut host, &mut guest], "confirm edit after restart", |apps| {
        apps.iter().all(|app| room(app).revision == 5 && room(app).pending.is_none())
    });
    assert_eq!(document(&host).item(host_id).unwrap().bounds().x0, checkpoint.frame_x + 18.0);
    assert_eq!(fingerprint(document(&host)).unwrap(), fingerprint(document(&guest)).unwrap());
    assert_comment(&host, &checkpoint);
    assert_comment(&guest, &checkpoint);
    guest.run("file.save", json!({})).unwrap();
    let reopened_guest = reopened(root, "Guest");
    assert_eq!(fingerprint(document(&reopened_guest)).unwrap(), fingerprint(document(&host)).unwrap());
}

#[test]
#[ignore = "requires a macOS user Keychain session"]
fn native_room_recovery_reconnects_after_process_exit() {
    if let Some(root) = std::env::var_os(CHILD_ROOT).map(PathBuf::from) {
        let mut password = String::new();
        std::io::stdin().read_to_string(&mut password).unwrap();
        let mut keychain = SecKeychain::open(root.join("room.keychain")).unwrap();
        keychain.unlock(Some(&password)).unwrap();
        with_test_keychain(keychain, || restore_checkpoint(&root));
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let password = Identity::generate().actor().to_string();
    let keychain_path = root.path().join("room.keychain");
    let mut keychain = CreateOptions::new().password(&password).create(&keychain_path).unwrap();
    let _cleanup = DisposableKeychain(keychain_path);
    keychain.unlock(Some(&password)).unwrap();
    with_test_keychain(keychain, || save_checkpoint(root.path()));

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "collaboration::tests::restart::native_room_recovery_reconnects_after_process_exit"])
        .env(CHILD_ROOT, root.path())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(password.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());

    // A libtest invocation with an unmatched filter can also exit successfully.
    // Verify the child actually saved the recovered room's two final edits.
    let checkpoint = read_checkpoint(root.path());
    let guest = reopened(root.path(), "Guest");
    let [frame] = document(&guest).spreads[0].items.as_slice() else {
        panic!("the child did not save the recovered frame");
    };
    assert_eq!(frame.bounds().x0, checkpoint.frame_x + 18.0);
}
