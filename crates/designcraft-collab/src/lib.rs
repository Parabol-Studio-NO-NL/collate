//! Versioned operations and the application-thread adapter for a seeded shared document.

use thiserror::Error;

pub mod artwork;
pub mod layers;
mod operations;
pub mod pages;
mod pdf_artwork;
mod replica;
pub mod text;
mod validation;

pub use operations::{Appearance, Change, CommentAnchor, CommentReply, CommentThread, Geometry, Operation, ShapeFrame, SharedId};
pub use replica::Replica;

/// The room protocol must identify the exact adapter schema used by all replicas.
pub const PROTOCOL_VERSION: u16 = 11;
pub const MAX_OPERATIONS: usize = 64;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("This edit includes changes that cannot be shared yet.")]
    UnsupportedEdit,
    #[error("This type of object cannot be edited in a shared document yet.")]
    UnsupportedObject,
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
