//! Bounded chunking for room edits that exceed one transport packet.

use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use designcraft_collab_core::MAX_PAYLOAD;
use sha2::{Digest, Sha256};

use super::attachments::{BlobCursor, Blobs};
use super::seed::{MAX_SEED_BYTES, SEED_CHUNK_BYTES, SeedAdvertisement, SeedChunk, SeedDownload};
use super::{Message, RoomProtocol, encode_for_protocol};

pub(super) struct WireEdit {
    bytes: Vec<u8>,
    protocol: RoomProtocol,
    offer: Option<SeedAdvertisement>,
    pub(super) blobs: Blobs,
    pub(super) artwork_size: u64,
}

#[derive(Clone, Copy, Default)]
pub(super) struct SendCursor(SendState, BlobCursor);

#[derive(Clone, Copy, Default)]
enum SendState {
    #[default]
    Start,
    Chunk(u32),
    Finished,
}

impl WireEdit {
    #[cfg(test)]
    pub(super) fn new(bytes: Vec<u8>) -> Result<Self, String> {
        Self::for_room(bytes, RoomProtocol::default())
    }

    pub(super) fn for_room(bytes: Vec<u8>, protocol: RoomProtocol) -> Result<Self, String> {
        if bytes.is_empty() || bytes.len() > MAX_SEED_BYTES {
            return Err("The shared edit has an unsupported size.".into());
        }
        let offer = (bytes.len() > MAX_PAYLOAD).then(|| SeedAdvertisement { length: bytes.len() as u32, digest: Sha256::digest(&bytes).into() });
        Ok(Self { bytes, protocol, offer, blobs: Blobs::new(), artwork_size: 0 })
    }

    pub(super) fn progress(&self, cursor: &SendCursor) -> Option<super::attachments::ArtworkProgress> {
        cursor.1.progress(&self.blobs)
    }

    /// Binds a receipt to the exact confirmed actor, sequence, operation metadata and blob IDs.
    pub(super) fn digest(&self) -> [u8; 32] {
        Sha256::digest(&self.bytes).into()
    }

    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Advance only after the transport accepts this frame. A rejected frame can be retried with the same cursor.
    pub(super) fn send_next(&self, cursor: &mut SendCursor, mut send: impl FnMut(Vec<u8>) -> Result<(), String>) -> Result<bool, String> {
        if let Some(sent) = cursor.1.next(&self.blobs, &mut send) {
            return sent.map(|_| false);
        }
        let (frame, next) = match (cursor.0, self.offer) {
            (SendState::Start, None) => (self.bytes.clone(), SendState::Finished),
            (SendState::Start, Some(offer)) => (encode_for_protocol(Message::EditOffer(offer), self.protocol)?, SendState::Chunk(0)),
            (SendState::Chunk(offset), Some(offer)) if offset < offer.length => {
                let end = (offset + SEED_CHUNK_BYTES).min(offer.length);
                let data = STANDARD_NO_PAD.encode(&self.bytes[offset as usize..end as usize]);
                let frame = encode_for_protocol(Message::EditPart(SeedChunk { offset, data }), self.protocol)?;
                let next = if end == offer.length { SendState::Finished } else { SendState::Chunk(end) };
                (frame, next)
            }
            _ => return Err("The shared edit transfer is no longer active.".into()),
        };
        send(frame)?;
        cursor.0 = next;
        Ok(matches!(next, SendState::Finished))
    }
}

pub(super) fn start(offer: SeedAdvertisement) -> Result<SeedDownload, String> {
    if offer.length as usize <= MAX_PAYLOAD {
        return Err("The shared edit has an invalid transfer size.".into());
    }
    SeedDownload::new(offer).map_err(|_| "The shared edit has an invalid transfer size.".into())
}

#[cfg(test)]
mod tests {
    use super::super::Packet;
    use super::*;

    #[test]
    fn one_packet_edit_retries_without_fragmenting() {
        let bytes = vec![27; MAX_PAYLOAD];
        let edit = WireEdit::new(bytes.clone()).unwrap();
        let mut cursor = SendCursor::default();

        assert!(edit.send_next(&mut cursor, |_| Err("queue full".into())).is_err());
        assert_eq!(
            edit.send_next(&mut cursor, |frame| {
                assert_eq!(frame, bytes);
                Ok(())
            }),
            Ok(true)
        );
        assert!(edit.send_next(&mut cursor, |_| panic!("completed edit sent again")).is_err());
    }

    #[test]
    fn fragmented_edit_retries_in_order_and_recovers_exact_bytes() {
        let bytes: Vec<_> = (0..MAX_PAYLOAD + 17).map(|index| (index % 253) as u8).collect();
        let edit = WireEdit::new(bytes.clone()).unwrap();
        let mut cursor = SendCursor::default();
        let mut rejected = Vec::new();
        let mut frames = Vec::new();

        assert_eq!(edit.len(), bytes.len());
        assert_eq!(
            edit.send_next(&mut cursor, |frame| {
                frames.push(frame);
                Ok(())
            }),
            Ok(false)
        );
        assert!(
            edit.send_next(&mut cursor, |frame| {
                rejected = frame;
                Err("queue full".into())
            })
            .is_err()
        );
        loop {
            if edit
                .send_next(&mut cursor, |frame| {
                    frames.push(frame);
                    Ok(())
                })
                .unwrap()
            {
                break;
            }
        }
        assert_eq!(frames[1], rejected);

        let mut packets = frames.into_iter().map(|frame| {
            assert!(frame.len() <= MAX_PAYLOAD);
            serde_json::from_slice::<Packet>(&frame).unwrap().message
        });
        let Some(Message::EditOffer(offer)) = packets.next() else { panic!("missing edit advertisement") };
        let mut download = start(offer).unwrap();
        let mut received = None;
        for packet in packets {
            let Message::EditPart(part) = packet else { panic!("unexpected edit packet") };
            assert!(received.is_none(), "transfer completed before the last frame");
            received = download.append(part).unwrap();
        }
        assert_eq!(received, Some(bytes));
    }

    #[test]
    fn send_and_receive_enforce_transfer_limits() {
        assert!(WireEdit::new(Vec::new()).is_err());
        assert!(WireEdit::new(vec![0; MAX_SEED_BYTES + 1]).is_err());
        for length in [MAX_PAYLOAD, MAX_SEED_BYTES + 1] {
            assert!(start(SeedAdvertisement { length: length as u32, digest: [0; 32] }).is_err());
        }
    }
}
