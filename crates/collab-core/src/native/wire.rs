use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use iroh::{
    EndpointAddr, SecretKey,
    endpoint::{RecvStream, SendStream},
};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::timeout;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::{ActorId, Peer};

pub(super) const ALPN: &[u8] = b"parabol/codesign-peers/1";
const PREFIX: &str = "codesign-peer:";
const APP_LINK: &str = "codesign-collab://join#";
const MAX_INVITATION: usize = 4096;
const MAX_FRAME: usize = 64 * 1024;
pub(super) const MAX_TICKET_ADDRESSES: usize = 16;
/// Bound connection setup and admission before a peer can use the room stream.
pub(super) const ADMISSION_TIMEOUT: Duration = Duration::from_secs(12);
/// Keep an admitted stream alive through brief relay throttling or artwork backpressure.
pub(super) const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Serialize, Deserialize)]
pub(super) struct Ticket {
    pub address: EndpointAddr,
    pub access: [u8; 32],
    pub relay: bool,
}

impl Ticket {
    pub fn new(address: EndpointAddr, relay: bool) -> Self {
        Self { address, access: SecretKey::generate().to_bytes(), relay }
    }

    pub fn parse(invitation: &str) -> Result<Self, String> {
        let error = || "Use an invitation from the Collate room host.".to_owned();
        let text = invitation.trim();
        let payload = text.strip_prefix(APP_LINK).unwrap_or(text).strip_prefix(PREFIX).ok_or_else(error)?;
        if payload.is_empty() || payload.len() > MAX_INVITATION {
            return Err(error());
        }
        let bytes = URL_SAFE_NO_PAD.decode(payload).map_err(|_| error())?;
        let ticket: Self = decode(&bytes).map_err(|_| error())?;
        if !ticket.valid() {
            return Err(error());
        }
        Ok(ticket)
    }

    pub fn code(&self) -> Result<String, String> {
        let payload = URL_SAFE_NO_PAD.encode(encode(self)?);
        if payload.len() > MAX_INVITATION || !self.valid() {
            return Err("The host does not have a supported address to share yet.".into());
        }
        Ok(format!("{PREFIX}{payload}"))
    }

    fn valid(&self) -> bool {
        (self.relay || self.address.ip_addrs().next().is_some())
            && self.address.addrs.len() <= MAX_TICKET_ADDRESSES
            && self.address.addrs.iter().all(|addr| addr.is_ip() || self.relay && addr.is_relay())
            && self.address.ip_addrs().all(|addr| addr.port() != 0 && !addr.ip().is_unspecified())
            && self.address.relay_urls().all(|url| iroh::endpoint::default_relay_mode().relay_map().contains(url))
            && self.access != [0; 32]
    }
}

pub(super) fn display_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 60 || name.chars().any(char::is_control) {
        return Err("Enter a display name of up to 60 characters.".into());
    }
    Ok(name.into())
}

#[derive(Serialize, Deserialize)]
pub(super) struct Hello {
    pub access: [u8; 32],
    pub name: String,
}

#[derive(Serialize, Deserialize)]
pub(super) enum Admit {
    Welcome { host: Peer, peers: Vec<Peer> },
    InvalidInvitation,
    Unavailable,
    AlreadyConnected,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) enum FromHost {
    PeerJoined(Peer),
    PeerLeft(ActorId),
    Message(Vec<u8>),
}

pub(super) type Input = FramedRead<RecvStream, LengthDelimitedCodec>;
pub(super) type Output = FramedWrite<SendStream, LengthDelimitedCodec>;

pub(super) fn framed((send, recv): (SendStream, RecvStream)) -> (Output, Input) {
    let mut codec = LengthDelimitedCodec::builder();
    codec.max_frame_length(MAX_FRAME);
    (codec.new_write(send), codec.new_read(recv))
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>, String> {
    postcard::to_allocvec(value).map_err(|_| "Could not encode the peer message.".into())
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    match postcard::take_from_bytes(bytes) {
        Ok((value, [])) => Ok(value),
        _ => Err("A peer sent an invalid message.".into()),
    }
}

pub(super) async fn read<T: serde::de::DeserializeOwned>(input: &mut Input) -> Result<T, String> {
    let frame = input.next().await.ok_or("The peer connection closed.")?.map_err(|_| "A peer message was interrupted.")?;
    decode(&frame)
}

pub(super) async fn write(output: &mut Output, value: &impl Serialize, deadline: Duration) -> Result<(), String> {
    let data = encode(value)?;
    timeout(deadline, output.send(data.as_slice()))
        .await
        .map_err(|_| "The peer stopped accepting messages.")?
        .map_err(|_| "The peer connection closed.".into())
}
