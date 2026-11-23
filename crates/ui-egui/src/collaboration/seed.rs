//! Chunking for the native document bytes sent over the authenticated room connection.

use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(super) const MAX_SEED_BYTES: usize = 4 * 1024 * 1024;
pub(super) const SEED_CHUNK_BYTES: u32 = 24 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SeedAdvertisement {
    pub length: u32,
    pub digest: [u8; 32],
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SeedChunk {
    pub offset: u32,
    pub data: String,
}

pub(super) struct SeedUpload {
    advertisement: SeedAdvertisement,
    bytes: Box<[u8]>,
}

impl SeedUpload {
    pub fn new(bytes: &[u8]) -> Result<Self, SeedError> {
        check_length(bytes.len())?;
        let advertisement = SeedAdvertisement { length: bytes.len() as u32, digest: Sha256::digest(bytes).into() };
        Ok(Self { advertisement, bytes: bytes.into() })
    }

    pub fn advertisement(&self) -> SeedAdvertisement {
        self.advertisement
    }

    pub fn chunk(&self, offset: u32) -> Option<SeedChunk> {
        if !offset.is_multiple_of(SEED_CHUNK_BYTES) || offset >= self.advertisement.length {
            return None;
        }
        let end = (offset + SEED_CHUNK_BYTES).min(self.advertisement.length);
        let data = STANDARD_NO_PAD.encode(&self.bytes[offset as usize..end as usize]);
        Some(SeedChunk { offset, data })
    }
}

pub(super) struct SeedDownload {
    advertisement: SeedAdvertisement,
    bytes: Option<Vec<u8>>,
}

impl SeedDownload {
    pub fn new(advertisement: SeedAdvertisement) -> Result<Self, SeedError> {
        check_length(advertisement.length as usize)?;
        Ok(Self { advertisement, bytes: Some(Vec::new()) })
    }

    /// The advertised length bounds allocation. Require full aligned chunks
    /// before the tail so an invitation cannot force arbitrarily many packets.
    pub fn append(&mut self, chunk: SeedChunk) -> Result<Option<Vec<u8>>, SeedError> {
        let bytes = self.bytes.as_mut().ok_or(SeedError::Finished)?;
        if chunk.offset as usize != bytes.len() {
            return Err(SeedError::Offset);
        }
        let length = self.advertisement.length as usize;
        let expected = (length - bytes.len()).min(SEED_CHUNK_BYTES as usize);
        if chunk.data.len() != (expected * 4).div_ceil(3) {
            return Err(SeedError::Chunk);
        }
        let part = STANDARD_NO_PAD.decode(chunk.data).map_err(|_| SeedError::Chunk)?;
        if part.len() != expected {
            return Err(SeedError::Chunk);
        }
        if bytes.is_empty() {
            bytes.reserve_exact(length);
        }
        bytes.extend_from_slice(&part);
        if bytes.len() < length {
            return Ok(None);
        }
        let complete = self.bytes.take().ok_or(SeedError::Finished)?;
        let digest: [u8; 32] = Sha256::digest(&complete).into();
        if digest != self.advertisement.digest {
            return Err(SeedError::Digest);
        }
        Ok(Some(complete))
    }
}

fn check_length(length: usize) -> Result<(), SeedError> {
    if length == 0 || length > MAX_SEED_BYTES {
        return Err(SeedError::Size);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SeedError {
    Size,
    Offset,
    Chunk,
    Digest,
    Finished,
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Size => "The shared starting document has an unsupported size.",
            Self::Offset => "An out-of-order document chunk was received.",
            Self::Chunk => "An invalid document chunk was received.",
            Self::Digest => "The received document failed verification.",
            Self::Finished => "A document chunk arrived after the transfer ended.",
        })
    }
}

impl std::error::Error for SeedError {}

#[cfg(test)]
mod tests {
    use super::{MAX_SEED_BYTES, SEED_CHUNK_BYTES, SeedAdvertisement, SeedDownload, SeedError, SeedUpload};

    #[test]
    fn a_chunked_seed_is_released_only_after_the_exact_tail() {
        let bytes: Vec<_> = (0..SEED_CHUNK_BYTES * 2 + 7).map(|position| (position % 253) as u8).collect();
        let upload = SeedUpload::new(&bytes).unwrap();
        let mut download = SeedDownload::new(upload.advertisement()).unwrap();

        for offset in [0, SEED_CHUNK_BYTES] {
            assert_eq!(download.append(upload.chunk(offset).unwrap()), Ok(None));
        }
        assert_eq!(download.append(upload.chunk(SEED_CHUNK_BYTES * 2).unwrap()), Ok(Some(bytes)));
        assert!(upload.chunk(upload.advertisement().length).is_none());
        assert_eq!(download.append(upload.chunk(0).unwrap()), Err(SeedError::Finished));
    }

    #[test]
    fn invalid_sizes_order_and_encoding_cannot_advance_the_download() {
        assert_eq!(SeedUpload::new(&[]).err(), Some(SeedError::Size));
        assert_eq!(SeedUpload::new(&vec![0; MAX_SEED_BYTES + 1]).err(), Some(SeedError::Size));
        let too_large = SeedAdvertisement { length: MAX_SEED_BYTES as u32 + 1, digest: [0; 32] };
        assert_eq!(SeedDownload::new(too_large).err(), Some(SeedError::Size));

        let bytes = vec![123; SEED_CHUNK_BYTES as usize + 5];
        let upload = SeedUpload::new(&bytes).unwrap();
        let mut download = SeedDownload::new(upload.advertisement()).unwrap();
        assert_eq!(download.append(upload.chunk(SEED_CHUNK_BYTES).unwrap()), Err(SeedError::Offset));
        assert!(upload.chunk(1).is_none());

        let mut oversized = upload.chunk(0).unwrap();
        oversized.data.push('A');
        assert_eq!(download.append(oversized), Err(SeedError::Chunk));
        let mut malformed = upload.chunk(0).unwrap();
        malformed.data.replace_range(..1, "!");
        assert_eq!(download.append(malformed), Err(SeedError::Chunk));
        assert_eq!(download.append(upload.chunk(0).unwrap()), Ok(None));
        assert_eq!(download.append(upload.chunk(0).unwrap()), Err(SeedError::Offset));
        assert_eq!(download.append(upload.chunk(SEED_CHUNK_BYTES).unwrap()), Ok(Some(bytes)));
    }

    #[test]
    fn a_wrong_fingerprint_never_yields_document_bytes() {
        let upload = SeedUpload::new(b"fixture document").unwrap();
        let mut advertisement = upload.advertisement();
        advertisement.digest[0] ^= 1;
        let mut download = SeedDownload::new(advertisement).unwrap();
        assert_eq!(download.append(upload.chunk(0).unwrap()), Err(SeedError::Digest));
        assert_eq!(download.append(upload.chunk(0).unwrap()), Err(SeedError::Finished));
    }
}
