mod host;
mod wire;

use iroh::{
    Endpoint, EndpointId, RelayMode, SecretKey,
    endpoint::{Connection, presets},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::{
    runtime::Builder,
    sync::mpsc,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;

use crate::{ActorId, Peer};

use self::wire::{ADMISSION_TIMEOUT, ALPN, Admit, FromHost, Hello, Ticket, WRITE_STALL_TIMEOUT};

pub const MAX_PAYLOAD: usize = 48 * 1024;
const MAX_GUESTS: usize = 3;
const QUEUE_CAPACITY: usize = 64;

#[derive(Clone)]
pub struct Identity(SecretKey);

impl Identity {
    pub fn generate() -> Self {
        Self(SecretKey::generate())
    }

    /// Restore a transport identity from the application's secure storage.
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        Self(SecretKey::from_bytes(bytes))
    }

    /// Private key bytes for handing the identity to application-owned secure storage.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn actor(&self) -> ActorId {
        actor(self.0.public())
    }
}

pub enum Event {
    Invitation(String),
    Ready {
        local: Peer,
        host: ActorId,
    },
    PeerJoined(Peer),
    PeerLeft(ActorId),
    /// `actor` is authenticated by the transport. Guests receive application data from the host.
    Message {
        actor: ActorId,
        bytes: Vec<u8>,
    },
    Disconnected,
    Failed(String),
}

/// One host or guest session. Dropping it closes its network connection.
pub struct Session {
    hosted: bool,
    generation: Arc<AtomicU64>,
    routes: host::Routes,
    outgoing: mpsc::Sender<Outgoing>,
    events: mpsc::Receiver<Event>,
    stopping: CancellationToken,
}

enum Mode {
    Host { relay: bool, port: Option<u16>, access: Option<[u8; 32]> },
    Guest(Ticket),
}

struct Outgoing {
    generation: u64,
    bytes: Vec<u8>,
}

impl Session {
    pub fn host(display_name: &str, relay: bool) -> Result<Self, String> {
        Self::host_with_identity(display_name, relay, Identity::generate())
    }

    pub fn host_with_identity(display_name: &str, relay: bool, identity: Identity) -> Result<Self, String> {
        Self::launch(display_name, identity, Mode::Host { relay, port: None, access: None })
    }

    /// Bind a dedicated host to a predictable UDP port for office/VPS firewalls.
    pub fn persistent_host(display_name: &str, relay: bool, identity: Identity, port: u16, access: [u8; 32]) -> Result<Self, String> {
        Self::launch(display_name, identity, Mode::Host { relay, port: Some(port), access: Some(access) })
    }

    pub fn join(invitation: &str, display_name: &str) -> Result<Self, String> {
        Self::join_with_identity(invitation, display_name, Identity::generate())
    }

    /// Read the host endpoint expected by an invitation without opening a connection.
    pub fn invitation_host(invitation: &str) -> Result<ActorId, String> {
        Ticket::parse(invitation).map(|ticket| actor(ticket.address.id))
    }

    pub fn join_with_identity(invitation: &str, display_name: &str, identity: Identity) -> Result<Self, String> {
        Self::launch(display_name, identity, Mode::Guest(Ticket::parse(invitation)?))
    }

    fn launch(display_name: &str, identity: Identity, mode: Mode) -> Result<Self, String> {
        let local = Peer { actor: identity.actor(), name: wire::display_name(display_name)? };
        let hosted = matches!(mode, Mode::Host { .. });
        let (outgoing, inbox) = mpsc::channel(QUEUE_CAPACITY);
        let (report, events) = mpsc::channel(QUEUE_CAPACITY);
        let stopping = CancellationToken::new();
        let generation = Arc::new(AtomicU64::new(0));
        let routes = host::Routes::default();
        let worker = Worker { inbox, report, routes: routes.clone(), stopping: stopping.clone(), generation: generation.clone() };
        let runtime = Builder::new_current_thread().enable_all().build().map_err(|_| "Could not initialize peer networking.")?;
        thread::Builder::new()
            .name("codesign-peers".into())
            .spawn(move || runtime.block_on(worker.run(local, identity, mode)))
            .map_err(|_| "Could not start peer networking.")?;
        Ok(Self { hosted, generation, routes, outgoing, events, stopping })
    }

    pub fn try_event(&mut self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Submit a proposal to the host. It stays private to the host until the app validates it.
    pub fn submit(&self, bytes: Vec<u8>) -> Result<(), String> {
        if self.hosted {
            return Err("The host applies its own application messages locally.".into());
        }
        self.send(bytes)
    }

    /// Send an application-approved message to every currently connected guest.
    pub fn broadcast(&self, bytes: Vec<u8>) -> Result<(), String> {
        self.send_as_host(None, bytes)
    }

    /// Send a response, checkpoint or replay to one guest.
    pub fn send_to(&self, peer: ActorId, bytes: Vec<u8>) -> Result<(), String> {
        self.send_as_host(Some(peer), bytes)
    }

    fn send_as_host(&self, peer: Option<ActorId>, bytes: Vec<u8>) -> Result<(), String> {
        if !self.hosted {
            return Err("Only the host can send approved room messages.".into());
        }
        if self.generation.load(Ordering::Acquire) == 0 {
            return Err("The peer session is offline.".into());
        }
        if bytes.is_empty() || bytes.len() > MAX_PAYLOAD {
            return Err("The application message has an invalid size.".into());
        }
        self.routes.send(peer, bytes)
    }

    fn send(&self, bytes: Vec<u8>) -> Result<(), String> {
        if bytes.is_empty() || bytes.len() > MAX_PAYLOAD {
            return Err("The application message has an invalid size.".into());
        }
        let generation = self.generation.load(Ordering::Acquire);
        if generation == 0 {
            return Err("The peer session is offline.".into());
        }
        self.outgoing.try_send(Outgoing { generation, bytes }).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => "The peer session is catching up.".into(),
            mpsc::error::TrySendError::Closed(_) => "The peer session is offline.".into(),
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stopping.cancel();
    }
}

struct Worker {
    routes: host::Routes,
    inbox: mpsc::Receiver<Outgoing>,
    report: mpsc::Sender<Event>,
    stopping: CancellationToken,
    generation: Arc<AtomicU64>,
}

impl Worker {
    async fn run(mut self, local: Peer, identity: Identity, mode: Mode) {
        let result = self.connect(local, identity, mode).await;
        self.generation.store(0, Ordering::Release);
        if let Err(reason) = result {
            self.publish(Event::Failed(reason)).await;
        }
    }

    async fn connect(&mut self, local: Peer, identity: Identity, mode: Mode) -> Result<(), String> {
        let (relay, port) = match &mode {
            Mode::Host { relay, port, .. } => (*relay, *port),
            Mode::Guest(ticket) => (ticket.relay, None),
        };
        let hosted = matches!(mode, Mode::Host { .. });
        let endpoint = bind(identity, relay, hosted, port).await?;
        let stop = self.stopping.clone();
        let result = tokio::select! {
            _ = stop.cancelled() => Ok(()),
            result = async { match mode {
                Mode::Host { relay, access, .. } => host::run(self, &endpoint, local, relay, access).await,
                Mode::Guest(ticket) => self.guest(&endpoint, &ticket, &local).await,
            }} => result,
        };
        self.generation.store(0, Ordering::Release);
        endpoint.close().await;
        result
    }

    async fn publish(&self, event: Event) {
        tokio::select! {
            _ = self.stopping.cancelled() => {},
            _ = self.report.send(event) => {},
        }
    }

    async fn ready(&self, local: Peer, host: ActorId, generation: u64) {
        self.generation.store(generation, Ordering::Release);
        self.publish(Event::Ready { local, host }).await;
    }

    async fn guest(&mut self, endpoint: &Endpoint, ticket: &Ticket, local: &Peer) -> Result<(), String> {
        let mut generation = 0;
        let mut seat_retries = 0;
        loop {
            match timeout(ADMISSION_TIMEOUT, endpoint.connect(ticket.address.clone(), ALPN)).await {
                Ok(Ok(connection)) => {
                    generation += 1;
                    match self.exchange(ticket, local, connection, generation).await {
                        ExchangeError::Rejected(message) => return Err(message),
                        ExchangeError::Retry => seat_retries = 0,
                        ExchangeError::AlreadyConnected if seat_retries < 3 => seat_retries += 1,
                        ExchangeError::AlreadyConnected => {
                            return Err("Your identity is still connected. Close the previous room and try again.".into());
                        }
                    }
                }
                _ if generation == 0 => return Err("Could not reach the room host. Ask for a current invitation and try again.".into()),
                _ => {}
            }
            self.generation.store(0, Ordering::Release);
            self.publish(Event::Disconnected).await;
            sleep(Duration::from_secs(2)).await;
        }
    }

    async fn exchange(&mut self, ticket: &Ticket, local: &Peer, connection: Connection, generation: u64) -> ExchangeError {
        let opened = timeout(ADMISSION_TIMEOUT, async {
            let streams = connection.open_bi().await.map_err(|_| "The peer stream closed.")?;
            let (mut output, mut input) = wire::framed(streams);
            wire::write(&mut output, &Hello { access: ticket.access, name: local.name.clone() }, ADMISSION_TIMEOUT).await?;
            let response = wire::read(&mut input).await?;
            Ok::<_, String>((output, input, response))
        })
        .await;
        let Ok(Ok((mut output, mut input, response))) = opened else {
            return ExchangeError::Retry;
        };
        let expected_host = actor(connection.remote_id());
        let (host, peers) = match response {
            Admit::Welcome { host, peers } if valid_roster(&host, &peers, expected_host, local.actor) => (host, peers),
            Admit::Welcome { .. } => return ExchangeError::Rejected("The host sent an invalid room roster.".into()),
            Admit::InvalidInvitation => return ExchangeError::Rejected("The invitation no longer matches the host.".into()),
            Admit::Unavailable => return ExchangeError::Rejected("The room has no seat available for this connection.".into()),
            Admit::AlreadyConnected => return ExchangeError::AlreadyConnected,
        };
        self.ready(local.clone(), host.actor, generation).await;
        self.publish(Event::PeerJoined(host.clone())).await;
        for peer in peers {
            self.publish(Event::PeerJoined(peer)).await;
        }
        let report = &self.report;
        let receive = async {
            while let Some(event) = wire::read(&mut input).await.ok().and_then(|value| guest_event(value, host.actor, local.actor)) {
                if report.send(event).await.is_err() {
                    break;
                }
            }
        };
        let transmit = async {
            while let Some(message) = self.inbox.recv().await {
                if message.generation == generation && wire::write(&mut output, &message.bytes, WRITE_STALL_TIMEOUT).await.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            _ = receive => {},
            _ = transmit => {},
        }
        ExchangeError::Retry
    }
}

enum ExchangeError {
    Rejected(String),
    Retry,
    AlreadyConnected,
}

fn guest_event(message: FromHost, host: ActorId, local: ActorId) -> Option<Event> {
    match message {
        FromHost::PeerJoined(peer) if peer.actor != host && peer.actor != local && wire::display_name(&peer.name).is_ok() => {
            Some(Event::PeerJoined(peer))
        }
        FromHost::PeerLeft(peer) if peer != host && peer != local => Some(Event::PeerLeft(peer)),
        FromHost::Message(bytes) if !bytes.is_empty() && bytes.len() <= MAX_PAYLOAD => Some(Event::Message { actor: host, bytes }),
        _ => None,
    }
}

fn valid_roster(host: &Peer, peers: &[Peer], expected_host: ActorId, local: ActorId) -> bool {
    if host.actor != expected_host || wire::display_name(&host.name).is_err() || peers.len() >= MAX_GUESTS {
        return false;
    }
    let mut distinct = std::collections::HashSet::from([local, expected_host]);
    peers.iter().all(|peer| distinct.insert(peer.actor) && wire::display_name(&peer.name).is_ok())
}

async fn bind(identity: Identity, relay: bool, host: bool, port: Option<u16>) -> Result<Endpoint, String> {
    let builder = if relay { Endpoint::builder(presets::N0) } else { Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled) };
    let builder = if let Some(port) = port {
        builder
            .clear_ip_transports()
            .bind_addr((std::net::Ipv4Addr::UNSPECIFIED, port))
            .and_then(|b| b.bind_addr_with_opts((std::net::Ipv6Addr::UNSPECIFIED, port), iroh::endpoint::BindOpts::default().set_is_required(false)))
            .map_err(|_| "Invalid host bind address.")?
    } else {
        builder
    };
    let builder = if host { builder.alpns(vec![ALPN.into()]) } else { builder };
    builder.secret_key(identity.0).bind().await.map_err(|_| "Could not start peer networking.".into())
}

fn actor(id: EndpointId) -> ActorId {
    ActorId(*id.as_bytes())
}

#[cfg(test)]
mod tests;
