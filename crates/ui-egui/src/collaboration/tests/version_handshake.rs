use super::*;
use designcraft_collab_core::{Event, Identity, Session as Transport};

#[test]
fn clipboard_only_peer_is_refused_before_receiving_the_room() {
    let seed = Document::new(&NewDocument::default());
    let mut host = editor(&seed, "Studio");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "room opens", |apps| room(apps[0]).invitation.is_some());

    let identity = Identity::generate();
    let actor = identity.actor();
    let mut guest = Transport::join_with_identity(room(&host).invitation.as_deref().unwrap(), "Older editor", identity).unwrap();
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
