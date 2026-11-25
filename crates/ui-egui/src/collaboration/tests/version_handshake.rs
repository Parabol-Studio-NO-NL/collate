use super::*;
use designcraft_collab::RoomProtocol;
use designcraft_collab_core::{Event, Identity, Session as Transport};

#[test]
fn clipboard_only_peer_is_refused_before_receiving_the_room() {
    let seed = Document::new(&NewDocument::default());
    let mut host = editor(&seed, "Studio");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "room opens", |apps| room(apps[0]).invitation.is_some());

    let identity = Identity::generate();
    let actor = identity.actor();
    let (_, ticket) = RoomProtocol::parse_invitation(room(&host).invitation.as_deref().unwrap());
    let mut guest = Transport::join_with_identity(ticket, "Older editor", identity).unwrap();
    // Version 14 is the clipboard schema. Keep its actual wire format independent
    // of the current encoder so accidentally reusing its version fails the handshake test.
    let hello = json!({"version": 14, "message": {"type": "hello", "data": {"seed": room(&host).seed, "revision": 0}}});
    wait_for(&mut [&mut host], "older guest receives an incompatibility reply", |_| {
        while let Some(event) = guest.try_event() {
            match event {
                Event::Ready { .. } => guest.submit(serde_json::to_vec(&hello).unwrap()).unwrap(),
                Event::Message { bytes, .. } => {
                    let reply: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(reply["message"]["type"], "incompatible");
                    assert_ne!(reply["version"], hello["version"]);
                    return true;
                }
                Event::Failed(error) => panic!("guest failed during handshake: {error}"),
                _ => {}
            }
        }
        false
    });
    assert!(!room(&host).serving(actor));
    assert!(room(&host).can_edit());
    assert_eq!(document(&host), &seed);
}

#[cfg(feature = "automerge-rooms")]
#[test]
fn the_same_build_joins_legacy_and_automerge_rooms() {
    use super::super::{Message, OpenRoom, Packet};

    let seed = Document::new(&NewDocument::default());
    for protocol in [RoomProtocol::Legacy, RoomProtocol::Automerge, RoomProtocol::AutomergeArtwork] {
        let mut host = editor(&seed, "Host");
        let created = host.run("frame.create", json!({"rect":[30,40,280,180],"content":"text","text":"Proof"})).unwrap();
        let story = StoryId(created["story"].as_u64().unwrap());
        host.connect_to_room(true, OpenRoom::Active, Some(protocol)).unwrap();
        wait_for(&mut [&mut host], "host advertises its room version", |apps| room(apps[0]).invitation.is_some());
        let invitation = room(&host).invitation.clone().unwrap();
        let (announced, ticket) = RoomProtocol::parse_invitation(&invitation);
        assert_eq!(announced, protocol);
        if protocol == RoomProtocol::Legacy {
            assert_eq!(ticket, invitation);
        }

        let mut guest = DesignApp::new(Session::new(), Services::default());
        guest.collaboration.name = "Guest".into();
        guest.collaboration.join_code = invitation;
        guest.start_seed_download().unwrap();
        wait_for(&mut [&mut host, &mut guest], "guest joins in the advertised protocol", |apps| {
            apps[1].collaboration.room.as_ref().is_some_and(Room::can_edit)
        });
        assert_eq!(room(&guest).protocol, protocol);
        guest.run("text.select", json!({"story":story.0,"anchor":5})).unwrap();
        guest.run("text.insert", json!({"text":"read","raw":true})).unwrap();
        wait_for(&mut [&mut host, &mut guest], "both guests see their story edit", |apps| apps.iter().all(|app| room(app).revision == 1));
        assert_eq!(document(&host).story(story).unwrap().text, "Proofread");
        assert_eq!(document(&host), document(&guest));
        let (commit, _) = &room(&host).history[0];
        assert_eq!(commit.proposal.story_transaction.is_some(), protocol.is_automerge());
        let wire = super::super::encode_for_protocol(Message::Commit(commit.clone()), protocol).unwrap();
        let decoded: Packet = serde_json::from_slice(&wire).unwrap();
        assert_eq!(decoded.version, protocol.version());
    }
}

#[test]
fn joining_an_older_host_reports_the_protocol_mismatch() {
    let seed = Document::new(&NewDocument::default());
    let mut host = editor(&seed, "Older host");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "room opens", |apps| room(apps[0]).invitation.is_some());
    let mut join = super::super::PendingJoin::start(room(&host).invitation.as_deref().unwrap(), "Newer editor").unwrap();
    let transport = &mut host.collaboration.room.as_mut().unwrap().transport;
    // Drive the host transport directly so it answers the real seed request as a v14 host.
    wait_for(&mut [], "guest reports the older host version", |_| {
        while let Some(event) = transport.try_event() {
            let Event::Message { actor, bytes } = event else { continue };
            let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request["message"]["type"], "requestSeed");
            let reply = json!({"version": 14, "message": {"type": "incompatible"}});
            transport.send_to(actor, serde_json::to_vec(&reply).unwrap()).unwrap();
        }
        match join.poll() {
            Ok(document) => {
                assert!(document.is_none());
                false
            }
            Err(message) => {
                assert!(message.contains("different collaboration protocol"), "{message}");
                true
            }
        }
    });
}
