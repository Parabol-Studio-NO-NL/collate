//! Versioned operations and the application-thread adapter for a seeded shared document.

use thiserror::Error;

pub mod artwork;
pub mod automerge_stories;
pub mod automerge_text;
pub mod layers;
mod operations;
pub mod pages;
mod pdf_artwork;
mod replica;
pub mod text;
mod validation;

pub use operations::{Appearance, Change, CommentAnchor, CommentReply, CommentThread, Geometry, InitialTextFormat, Operation, ShapeFrame, SharedId};
pub use replica::Replica;

/// A room selects its adapter schema when it starts. Every peer uses that schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoomProtocol {
    Legacy,
    Automerge,
    AutomergeArtwork,
}

impl Default for RoomProtocol {
    fn default() -> Self {
        Self::CURRENT
    }
}

impl RoomProtocol {
    pub const CURRENT: Self = if cfg!(feature = "automerge-rooms") { Self::AutomergeArtwork } else { Self::Legacy };
    const AUTOMERGE_INVITATION: &str = "collate-room-17:";
    const ARTWORK_INVITATION: &str = "collate-room-18:";

    pub const fn version(self) -> u16 {
        match self {
            Self::Legacy => 15,
            Self::Automerge => 17,
            Self::AutomergeArtwork => 18,
        }
    }

    pub const fn is_automerge(self) -> bool {
        matches!(self, Self::Automerge | Self::AutomergeArtwork)
    }

    pub const fn supports_print_artwork(self) -> bool {
        matches!(self, Self::AutomergeArtwork)
    }

    pub const fn is_supported(self) -> bool {
        !self.is_automerge() || cfg!(feature = "automerge-rooms")
    }

    pub const fn history_limit(self) -> usize {
        if self.is_automerge() { 16_384 } else { 1024 }
    }

    pub const fn history_byte_limit(self) -> usize {
        (if self.is_automerge() { 64 } else { 8 }) * 1024 * 1024
    }

    /// Existing invites remain byte-for-byte valid for legacy rooms.
    pub fn decorate_invitation(self, ticket: String) -> String {
        match self {
            Self::Legacy => ticket,
            Self::Automerge => format!("{}{ticket}", Self::AUTOMERGE_INVITATION),
            Self::AutomergeArtwork => format!("{}{ticket}", Self::ARTWORK_INVITATION),
        }
    }

    /// Split the application version from the underlying peer ticket.
    pub fn parse_invitation(invitation: &str) -> (Self, &str) {
        const APP_LINK: &str = "codesign-collab://join#";
        let invitation = invitation.trim();
        let bare = invitation.strip_prefix(APP_LINK).unwrap_or(invitation);
        if let Some(ticket) = bare.strip_prefix(Self::ARTWORK_INVITATION) {
            (Self::AutomergeArtwork, ticket)
        } else if let Some(ticket) = bare.strip_prefix(Self::AUTOMERGE_INVITATION) {
            (Self::Automerge, ticket)
        } else {
            (Self::Legacy, invitation)
        }
    }
}

pub const PROTOCOL_VERSION: u16 = RoomProtocol::CURRENT.version();
pub const MAX_OPERATIONS: usize = 64;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("This edit includes changes that cannot be shared yet.")]
    UnsupportedEdit,
    #[error("This type of object cannot be edited in a shared document yet.")]
    UnsupportedObject,
    #[error("Start a new room to share PSD or TIFF artwork.")]
    ArtworkNeedsNewRoom,
    #[error("The shared object is no longer available.")]
    UnknownObject,
    #[error("The shared edit points to a missing document resource.")]
    UnknownResource,
    #[error("The shared edit reuses an existing identifier.")]
    DuplicateId,
    #[error("The shared edit exceeds a collaboration limit.")]
    Limit,
    #[error("The shared edit has an invalid {0}.")]
    Invalid(&'static str),
}
