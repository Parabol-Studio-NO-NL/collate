use iroh::{Endpoint, EndpointAddr, endpoint::Incoming};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::{interval, timeout},
};
use tokio_util::sync::CancellationToken;

use crate::{ActorId, Peer};

use super::{Event, MAX_GUESTS, MAX_PAYLOAD, Outgoing, QUEUE_CAPACITY, Worker, actor, wire};
use wire::{Admit, FromHost, Hello, IO_TIMEOUT, MAX_TICKET_ADDRESSES, Ticket};

#[derive(Clone, Copy, Eq, PartialEq)]
struct ConnectionKey {
    actor: ActorId,
    connection: usize,
}

struct Seat {
    key: ConnectionKey,
    peer: Peer,
    mailbox: mpsc::Sender<FromHost>,
    stop: CancellationToken,
}

enum Request {
    Admit(Seat, oneshot::Sender<Admit>),
    Message(ConnectionKey, Vec<u8>),
}

pub(super) async fn run(worker: &mut Worker, endpoint: &Endpoint, local: Peer, relay: bool) -> Result<(), String> {
    if relay {
        let _ = timeout(Duration::from_secs(4), endpoint.online()).await;
    }
    let mut ticket = Ticket::new(advertised_address(endpoint), relay);
    let mut address_check = interval(Duration::from_secs(3));
    let (requests, mut incoming) = mpsc::channel(QUEUE_CAPACITY);
    let mut tasks = JoinSet::new();
    let mut room = Room { host: local.clone(), guests: BTreeMap::new() };
    worker.ready(local.clone(), local.actor, 1).await;
    if let Ok(code) = ticket.code() {
        worker.publish(Event::Invitation(code)).await;
    }
    loop {
        tokio::select! {
            _ = address_check.tick() => {
                let address = advertised_address(endpoint);
                if address != ticket.address {
                    ticket.address = address;
                    if let Ok(code) = ticket.code() {
                        worker.publish(Event::Invitation(code)).await;
                    }
                }
            }
            connection = endpoint.accept(), if tasks.len() < MAX_GUESTS + 4 => {
                let Some(connection) = connection else { return Err("The host endpoint closed.".into()); };
                tasks.spawn(serve(connection, ticket.access, requests.clone()));
            }
            task = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Ok(Some(key))) = task && room.contains(key) {
                    room.remove(worker, vec![key.actor]).await;
                }
            }
            Some(request) = incoming.recv() => room.receive(worker, request).await,
            Some(message) = worker.inbox.recv() => room.send(worker, message).await,
        }
    }
}

fn advertised_address(endpoint: &Endpoint) -> EndpointAddr {
    let mut address = endpoint.addr();
    for socket in endpoint.bound_sockets() {
        // A wildcard listener also accepts connections from the same Mac. Iroh
        // omits loopback from discovery when active LAN interfaces are present.
        if !socket.ip().is_unspecified() || address.addrs.len() >= MAX_TICKET_ADDRESSES {
            continue;
        }
        let ip = match socket {
            SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        };
        address = address.with_ip_addr(SocketAddr::new(ip, socket.port()));
    }
    address
}

struct Room {
    host: Peer,
    guests: BTreeMap<ActorId, Seat>,
}

impl Room {
    fn contains(&self, key: ConnectionKey) -> bool {
        self.guests.get(&key.actor).is_some_and(|seat| seat.key == key)
    }

    async fn receive(&mut self, worker: &Worker, request: Request) {
        match request {
            Request::Admit(seat, reply) => self.admit(worker, seat, reply).await,
            Request::Message(key, bytes) if self.contains(key) => worker.publish(Event::Message { actor: key.actor, bytes }).await,
            Request::Message(..) => {}
        }
    }

    async fn admit(&mut self, worker: &Worker, seat: Seat, reply: oneshot::Sender<Admit>) {
        let peer = seat.peer.clone();
        if self.guests.contains_key(&peer.actor) {
            let _ = reply.send(Admit::AlreadyConnected);
            return;
        }
        if self.guests.len() >= MAX_GUESTS || peer.actor == self.host.actor {
            let _ = reply.send(Admit::Unavailable);
            return;
        }
        let roster = self.guests.values().map(|guest| guest.peer.clone()).collect();
        if reply.send(Admit::Welcome { host: self.host.clone(), peers: roster }).is_ok() {
            let stale = self.deliver(&FromHost::PeerJoined(peer.clone()), None);
            self.guests.insert(peer.actor, seat);
            worker.publish(Event::PeerJoined(peer)).await;
            self.remove(worker, stale).await;
        }
    }

    async fn send(&mut self, worker: &Worker, message: Outgoing) {
        if message.generation == 1 {
            let stale = self.deliver(&FromHost::Message(message.bytes), message.peer);
            self.remove(worker, stale).await;
        }
    }

    fn deliver(&self, message: &FromHost, target: Option<ActorId>) -> Vec<ActorId> {
        self.guests
            .iter()
            .filter(|(id, _)| target.is_none_or(|target| target == **id))
            .filter_map(|(&id, seat)| seat.mailbox.try_send(message.clone()).err().map(|_| id))
            .collect()
    }

    async fn remove(&mut self, worker: &Worker, mut stale: Vec<ActorId>) {
        while let Some(actor) = stale.pop() {
            if let Some(seat) = self.guests.remove(&actor) {
                seat.stop.cancel();
                worker.publish(Event::PeerLeft(actor)).await;
                stale.extend(self.deliver(&FromHost::PeerLeft(actor), None));
            }
        }
    }
}

async fn serve(incoming: Incoming, access: [u8; 32], host: mpsc::Sender<Request>) -> Option<ConnectionKey> {
    let acknowledged = timeout(IO_TIMEOUT, async {
        let connection = incoming.await.ok()?;
        let (mut output, mut input) = wire::framed(connection.accept_bi().await.ok()?);
        let hello: Hello = wire::read(&mut input).await.ok()?;
        if !bool::from(hello.access.ct_eq(&access)) {
            reject(&mut output, Admit::InvalidInvitation).await;
            return None;
        }
        let name = wire::display_name(&hello.name).ok()?;
        let key = ConnectionKey { actor: actor(connection.remote_id()), connection: connection.stable_id() };
        Some((connection, output, input, key, name))
    })
    .await
    .ok()
    .flatten();
    let (connection, mut output, mut input, key, name) = acknowledged?;
    let (mailbox, mut outgoing) = mpsc::channel(QUEUE_CAPACITY);
    let stop = CancellationToken::new();
    let seat = Seat { key, peer: Peer { actor: key.actor, name }, mailbox, stop: stop.clone() };
    let (reply, admission) = oneshot::channel();
    host.send(Request::Admit(seat, reply)).await.ok()?;
    match admission.await.ok()? {
        welcome @ Admit::Welcome { .. } => {
            if wire::write(&mut output, &welcome).await.is_ok() {
                tokio::select! {
                    _ = stop.cancelled() => {},
                    _ = exchange(&mut input, &mut output, &mut outgoing, &host, key) => {},
                }
            }
            connection.close(0u32.into(), b"room connection ended");
            Some(key)
        }
        denied => {
            reject(&mut output, denied).await;
            None
        }
    }
}

async fn reject(output: &mut wire::Output, response: Admit) {
    if wire::write(output, &response).await.is_ok() {
        let _ = output.get_mut().finish();
        let _ = timeout(Duration::from_secs(1), output.get_mut().stopped()).await;
    }
}

async fn exchange(
    input: &mut wire::Input,
    output: &mut wire::Output,
    outgoing: &mut mpsc::Receiver<FromHost>,
    host: &mpsc::Sender<Request>,
    key: ConnectionKey,
) -> Option<()> {
    loop {
        tokio::select! {
            proposal = wire::read::<Vec<u8>>(input) => {
                let bytes = proposal.ok().filter(|bytes| !bytes.is_empty() && bytes.len() <= MAX_PAYLOAD)?;
                host.send(Request::Message(key, bytes)).await.ok()?;
            }
            message = outgoing.recv() => wire::write(output, &message?).await.ok()?,
        }
    }
}
