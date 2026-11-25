//! Large binary artwork uses the authenticated room connection. One ordered, bounded stream
//! precedes its seed or edit; ordinary packets and per-peer flow control stay small.
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, SeekFrom, Write},
    sync::Arc,
};

use designcraft_collab::artwork::{BlobId, MAX_ARTWORK_BYTES};
use designcraft_collab_core::MAX_PAYLOAD;
use sha2::{Digest, Sha256};
use tempfile::SpooledTempFile;

use super::Proposal;

pub(super) type Blobs = BTreeMap<BlobId, Arc<Vec<u8>>>;
const PREFIX: u8 = 0xb4;
const HEADER: usize = 1 + 32 + 4 + 4;
const PART: usize = MAX_PAYLOAD - HEADER;
const INVALID: &str = "The shared artwork transfer is invalid or exceeds the 1 GiB room limit.";
const IO_ERROR: &str = "Could not stage shared artwork. Check available disk space.";

pub(super) const ARTWORK_LIMIT: &str =
    "This room has reached its 1 GiB artwork limit. Place a smaller file, or start a new room from a saved document with fewer embedded images.";

pub(super) struct ArtworkProgress {
    pub(super) sending: bool,
    pub(super) bytes: usize,
    pub(super) total: usize,
}

impl Proposal {
    pub(super) fn artwork_size(&self) -> u64 {
        let bodies: u64 = self.blob_refs().into_iter().map(|id| u64::from(id.length)).sum();
        // Older records carry a separate inline copy on every placement.
        let inline: u64 = self.edits.iter().filter_map(|edit| edit.placement()).filter(|p| p.blob().is_none()).map(|p| p.artwork_size()).sum();
        bodies + inline
    }

    pub(super) fn blob_refs(&self) -> BTreeSet<BlobId> {
        self.edits.iter().filter_map(|edit| edit.placement()?.blob().map(|(id, _)| id)).collect()
    }

    pub(super) fn blobs(&self) -> Result<Blobs, String> {
        self.edits.iter().filter_map(|e| e.placement()?.blob()).map(|(id, body)| Ok((id, body.ok_or(INVALID)?.clone()))).collect()
    }

    pub(super) fn attach(&mut self, blobs: &Blobs) -> Result<(), String> {
        for placement in self.edits.iter_mut().filter_map(|e| e.placement_mut()) {
            if let Some((id, _)) = placement.blob() {
                placement.attach_verified(blobs.get(&id).ok_or(INVALID)?.clone()).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

pub(super) fn is_part(bytes: &[u8]) -> bool {
    bytes.first() == Some(&PREFIX)
}

#[derive(Clone, Copy, Default)]
pub(super) struct BlobCursor {
    index: usize,
    offset: u32,
}

impl BlobCursor {
    pub(super) fn progress(&self, blobs: &Blobs) -> Option<ArtworkProgress> {
        let (id, _) = blobs.iter().nth(self.index)?;
        Some(ArtworkProgress { sending: true, bytes: self.offset as usize, total: id.length as usize })
    }

    pub(super) fn finished(&self, blobs: &Blobs) -> bool {
        self.index >= blobs.len()
    }

    /// None after all attachments have been accepted by the transport queue.
    pub(super) fn next(&mut self, blobs: &Blobs, send: impl FnOnce(Vec<u8>) -> Result<(), String>) -> Option<Result<(), String>> {
        let (id, data) = blobs.iter().nth(self.index)?;
        let start = self.offset as usize;
        let end = (start + PART).min(data.len());
        let mut frame = Vec::with_capacity(HEADER + end - start);
        frame.push(PREFIX);
        frame.extend(id.digest);
        frame.extend(id.length.to_be_bytes());
        frame.extend(self.offset.to_be_bytes());
        frame.extend_from_slice(&data[start..end]);
        Some(send(frame).inspect(|_| {
            if end == data.len() {
                self.index += 1;
                self.offset = 0;
            } else {
                self.offset = end as u32;
            }
        }))
    }
}

struct Incoming {
    id: BlobId,
    file: SpooledTempFile,
    received: usize,
    hash: Sha256,
}

#[derive(Default)]
pub(super) struct BlobDownload {
    pending: Option<Incoming>,
    complete: BTreeMap<BlobId, SpooledTempFile>,
    reserved: usize,
    last_part: Option<web_time::Instant>,
}

impl BlobDownload {
    pub(super) fn progress(&self) -> Option<ArtworkProgress> {
        if let Some(stream) = &self.pending {
            return Some(ArtworkProgress { sending: false, bytes: stream.received, total: stream.id.length as usize });
        }
        let id = self.complete.keys().next_back()?;
        Some(ArtworkProgress { sending: false, bytes: id.length as usize, total: id.length as usize })
    }

    pub(super) fn expired(&self) -> bool {
        self.last_part.is_some_and(|last| last.elapsed() >= std::time::Duration::from_secs(90))
    }

    pub(super) fn reserved(&self) -> usize {
        self.reserved
    }

    /// Reserve by declared length before accepting any bytes. The caller deducts all
    /// competing transfers from the allowance. Disk space grows only as bytes arrive.
    pub(super) fn append(&mut self, bytes: &[u8], allowance: usize) -> Result<(), String> {
        if !is_part(bytes) || !(HEADER + 1..=MAX_PAYLOAD).contains(&bytes.len()) {
            return Err(INVALID.into());
        }
        let digest = bytes[1..33].try_into().map_err(|_| INVALID)?;
        let length = u32::from_be_bytes(bytes[33..37].try_into().map_err(|_| INVALID)?);
        let offset = u32::from_be_bytes(bytes[37..41].try_into().map_err(|_| INVALID)?) as usize;
        let id = BlobId { digest, length };
        let length = length as usize;
        if !id.valid() || offset >= length || bytes.len() - HEADER != PART.min(length - offset) {
            return Err(INVALID.into());
        }
        if self.pending.is_none() {
            if offset != 0 || self.complete.contains_key(&id) || self.complete.len() >= 256 || self.reserved.saturating_add(length) > allowance {
                return Err(INVALID.into());
            }
            // A zero threshold keeps every nonempty chunk on disk in an anonymous temporary file.
            self.pending = Some(Incoming { id, file: tempfile::spooled_tempfile(0), received: 0, hash: Sha256::new() });
            self.reserved += length;
        }
        let stream = self.pending.as_mut().ok_or(INVALID)?;
        if stream.id != id || stream.received != offset {
            return Err(INVALID.into());
        }
        let data = &bytes[HEADER..];
        stream.file.write_all(data).map_err(|_| IO_ERROR)?;
        stream.hash.update(data);
        stream.received += data.len();
        self.last_part = Some(web_time::Instant::now());
        if stream.received == length {
            let stream = self.pending.take().ok_or(INVALID)?;
            if <[u8; 32]>::from(stream.hash.finalize()) != digest {
                return Err(INVALID.into());
            }
            self.complete.insert(id, stream.file);
        }
        Ok(())
    }

    pub(super) fn load(&mut self, id: BlobId) -> Result<Arc<Vec<u8>>, String> {
        let mut file = self.complete.remove(&id).ok_or(INVALID)?;
        file.seek(SeekFrom::Start(0)).map_err(|_| IO_ERROR)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(id.length as usize).map_err(|_| IO_ERROR)?;
        file.take(u64::from(id.length)).read_to_end(&mut bytes).map_err(|_| IO_ERROR)?;
        if bytes.len() != id.length as usize {
            return Err(INVALID.into());
        }
        Ok(Arc::new(bytes))
    }

    pub(super) fn finish(&self) -> Result<(), String> {
        if self.complete.is_empty() && self.pending.is_none() { Ok(()) } else { Err(INVALID.into()) }
    }

    pub(super) fn attach(mut self, proposal: &mut Proposal, pending: Option<&Proposal>) -> Result<(), String> {
        let cached = pending.map(Proposal::blobs).transpose()?.unwrap_or_default();
        let mut blobs = Blobs::new();
        for id in proposal.blob_refs() {
            // An acknowledgement can use the exact immutable allocation the guest
            // already owns. The staged file still has to pass its digest check.
            let body = if let Some(body) = cached.get(&id) {
                self.complete.remove(&id).ok_or(INVALID)?;
                body.clone()
            } else {
                self.load(id)?
            };
            blobs.insert(id, body);
        }
        self.finish()?;
        proposal.attach(&blobs)
    }
}

pub(super) fn total(blobs: &Blobs) -> Result<usize, String> {
    let total = blobs.keys().map(|id| u64::from(id.length)).sum::<u64>();
    if total > MAX_ARTWORK_BYTES as u64 {
        return Err(INVALID.into());
    }
    Ok(total as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use designcraft_collab::{Operation, SharedId};
    use designcraft_doc::ItemId;
    use serde_json::json;

    #[test]
    fn repeated_placements_share_the_attachment_budget_but_retain_each_legacy_inline_copy() {
        let graphic = |item, digest, artwork| Operation::SetGraphic {
            id: SharedId::Seed(ItemId(item)),
            placement: Some(Box::new(
                serde_json::from_value(json!({
                    "digest": vec![digest; 32], "size": [1, 1], "transform": [1, 0, 0, 1, 0, 0], "artwork": artwork
                }))
                .unwrap(),
            )),
        };
        let size: u64 = 600 * 1024 * 1024;
        let body = json!({"name": "photo.png", "mime": "image/png", "length": size});
        let legacy = json!({"name": "old.png", "mime": "image/png", "bytes": "YWJj"});
        let mut proposal = Proposal {
            base: 0,
            sequence: 1,
            history_action: None,
            edits: vec![graphic(1, 1, body.clone()), graphic(2, 1, body.clone()), graphic(3, 2, legacy.clone()), graphic(4, 2, legacy)],
        };
        assert_eq!(proposal.artwork_size(), size + 2 * b"abc".len() as u64);
        assert_eq!(proposal.blob_refs().len(), 1);
        proposal.edits.push(graphic(5, 3, body));
        assert!(proposal.artwork_size() > MAX_ARTWORK_BYTES as u64, "distinct files of the same length must count separately");
    }

    #[test]
    fn staged_artwork_rejects_reordering_corruption_and_overcommit() {
        let body = Arc::new((0..(PART + 113)).map(|i| (i % 239) as u8).collect::<Vec<_>>());
        let id = BlobId { length: body.len() as u32, digest: Sha256::digest(body.as_slice()).into() };
        let blobs = BTreeMap::from([(id, body.clone())]);
        let mut cursor = BlobCursor::default();
        let mut frames = Vec::new();
        while let Some(result) = cursor.next(&blobs, |frame| {
            frames.push(frame);
            Ok(())
        }) {
            result.unwrap();
        }

        let mut download = BlobDownload::default();
        assert!(download.append(&frames[0], body.len() - 1).is_err());
        assert!(download.append(&frames[1], body.len()).is_err());
        download.append(&frames[0], body.len()).unwrap();
        assert!(download.load(id).is_err());
        let mut corrupt = frames[1].clone();
        *corrupt.last_mut().unwrap() ^= 7;
        assert!(download.append(&corrupt, body.len()).is_err());
        assert!(download.load(id).is_err());

        let mut retry = BlobDownload::default();
        for frame in frames {
            retry.append(&frame, body.len()).unwrap();
        }
        assert!(retry.finish().is_err(), "unreferenced data cannot complete a transfer");
        assert_eq!(retry.load(id).unwrap(), body);
        retry.finish().unwrap();
    }
}
