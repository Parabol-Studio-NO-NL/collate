use std::{
    thread::sleep,
    time::{Duration, Instant},
};

use super::{Event, Identity, Session, Ticket};
use crate::ActorId;

fn until<T>(session: &mut Session, mut pick: impl FnMut(Event) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(18);
    loop {
        while let Some(event) = session.try_event() {
            if let Some(value) = pick(event) {
                return value;
            }
        }
        assert!(Instant::now() < deadline, "peer event did not arrive");
        sleep(Duration::from_millis(10));
    }
}

fn ready(session: &mut Session) -> (ActorId, ActorId) {
    until(session, |event| match event {
        Event::Ready { local, host } => Some((local.actor, host)),
        Event::Failed(reason) => panic!("peer failed: {reason}"),
        _ => None,
    })
}

fn message(session: &mut Session) -> (ActorId, Vec<u8>) {
    until(session, |event| match event {
        Event::Message { actor, bytes } => Some((actor, bytes)),
        Event::Failed(reason) => panic!("peer failed: {reason}"),
        _ => None,
    })
}

#[test]
fn local_room_authenticates_proposals_and_only_relays_host_messages() {
    let mut host = Session::host("Host", false).unwrap();
    let (host_actor, _) = ready(&mut host);
    let code = until(&mut host, |event| match event {
        Event::Invitation(code) => Some(code),
        Event::Failed(reason) => panic!("host failed: {reason}"),
        _ => None,
    });
    let mut ticket = Ticket::parse(&code).unwrap();
    assert_eq!(Session::invitation_host(&code).unwrap(), host_actor);
    assert!(!ticket.relay);
    ticket.access[0] ^= 1;
    let mut stranger = Session::join(&ticket.code().unwrap(), "Stranger").unwrap();
    let error = until(&mut stranger, |event| match event {
        Event::Ready { .. } => panic!("a peer without the room's bearer credential joined"),
        Event::Failed(error) => Some(error),
        _ => None,
    });
    assert!(error.contains("invitation"));

    let identity = Identity::generate();
    let expected_actor = identity.actor();
    let mut alice = Session::join_with_identity(&format!("codesign-collab://join#{code}"), "Alice", identity.clone()).unwrap();
    let (alice_actor, alice_host) = ready(&mut alice);
    assert_eq!((alice_actor, alice_host), (expected_actor, host_actor));
    let mut bob = Session::join(&code, "Bob").unwrap();
    let (bob_actor, bob_host) = ready(&mut bob);
    assert_eq!(bob_host, host_actor);
    let joined = until(&mut alice, |event| match event {
        Event::PeerJoined(peer) if peer.actor == bob_actor => Some(peer),
        _ => None,
    });
    assert_eq!(joined.name, "Bob");

    assert!(host.submit(vec![0]).is_err());
    assert!(alice.broadcast(vec![0]).is_err());
    alice.submit(vec![0, 1, 255]).unwrap();
    assert_eq!(message(&mut host), (alice_actor, vec![0, 1, 255]));
    host.send_to(alice_actor, vec![33]).unwrap();
    assert_eq!(message(&mut alice), (host_actor, vec![33]));
    host.broadcast(vec![42]).unwrap();
    assert_eq!(message(&mut bob), (host_actor, vec![42]));

    let mut replacement = Session::join_with_identity(&code, "Alice", identity).unwrap();
    until(&mut replacement, |event| match event {
        Event::Disconnected => Some(()),
        Event::Failed(reason) => panic!("retryable duplicate seat failed: {reason}"),
        Event::Ready { .. } => panic!("both connections have the same actor"),
        _ => None,
    });
    drop(alice);
    until(&mut host, |event| match event {
        Event::PeerLeft(actor) if actor == alice_actor => Some(()),
        _ => None,
    });
    assert_eq!(ready(&mut replacement), (alice_actor, host_actor));
}
