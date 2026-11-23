//! Native peer transport adapted from Parabol's Repro With Friends collaboration.
//! The application owns document authorization, validation, sequencing and persistence.

use serde::{Deserialize, Serialize};

/// Public identity of an authenticated transport endpoint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ActorId(pub [u8; 32]);

impl std::fmt::Display for ActorId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Peer {
    pub actor: ActorId,
    pub name: String,
}

#[cfg(all(feature = "native", not(target_arch = "wasm32")))]
mod native;

#[cfg(all(feature = "native", not(target_arch = "wasm32")))]
pub use native::{Event, Identity, MAX_PAYLOAD, Session};

#[cfg(all(feature = "secure-journal", target_os = "macos"))]
pub mod journal;
