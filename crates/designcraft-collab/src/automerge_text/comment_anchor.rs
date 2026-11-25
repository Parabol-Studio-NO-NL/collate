//! Text comment ranges refer to the characters that were visible in a saved Automerge view.
//! A comment's two byte offsets are only interpreted in that historical view.

use std::ops::Range;

use automerge::{ChangeHash, Cursor, ReadDoc, transaction::Transactable};
use serde::{Deserialize, Serialize};

use crate::{Error, text::MAX_EDIT_ATOMS};

use super::{AutomergeText, MAX_STORY_BYTES, invalid};

const MAX_HEADS: usize = 64;
const MAX_ANCHOR_BYTES: usize = MAX_EDIT_ATOMS * 4;

/// Compact description of the original characters selected for a comment.
/// The checkpoint or room replay must retain the referenced changes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutomergeCommentAnchor {
    #[serde(with = "frontier")]
    heads: Vec<ChangeHash>,
    start: u32,
    end: u32,
}

impl AutomergeText {
    /// Anchor the original characters in a nonempty UTF-8 byte range.
    pub fn comment_anchor(&self, range: Range<usize>) -> Result<AutomergeCommentAnchor, Error> {
        let mut heads = self.doc.base_heads();
        heads.sort_unstable();
        let anchor = AutomergeCommentAnchor {
            heads,
            start: u32::try_from(range.start).map_err(|_| Error::Limit)?,
            end: u32::try_from(range.end).map_err(|_| Error::Limit)?,
        };
        self.validate_comment_anchor(&anchor)?;
        Ok(anchor)
    }

    /// Verify that a peer's selection identifies a bounded historical range in this story.
    /// The original characters may already have been deleted from the current version.
    pub fn validate_comment_anchor(&self, anchor: &AutomergeCommentAnchor) -> Result<(), Error> {
        let range = self.comment_origin(anchor)?;
        let mut byte = range.start;
        let mut count = 0;
        while byte < range.end {
            if count == MAX_EDIT_ATOMS {
                return Err(Error::Limit);
            }
            let (_, origin) = self.historical_scalar(anchor, byte)?;
            if origin.start != byte {
                return Err(invalid_range());
            }
            byte = origin.end;
            count += 1;
        }
        if byte != range.end {
            return Err(invalid_range());
        }
        Ok(())
    }

    /// Return the span between the first and last surviving original characters.
    /// Later insertions may appear inside the span, but never become its anchors.
    pub fn comment_range(&self, anchor: &AutomergeCommentAnchor) -> Option<Range<usize>> {
        let range = self.comment_origin(anchor).ok()?;
        let mut byte = range.start;
        let mut first = None;
        for _ in 0..MAX_EDIT_ATOMS {
            if byte >= range.end {
                break;
            }
            let (cursor, origin) = self.historical_scalar(anchor, byte).ok()?;
            if origin.start != byte || origin.end > range.end {
                return None;
            }
            if let Some(current) = self.visible_scalar(&cursor, origin.len()) {
                first = Some(current);
                break;
            }
            byte = origin.end;
        }
        let first = first?;
        let mut byte = range.end;
        for _ in 0..MAX_EDIT_ATOMS {
            if byte <= range.start {
                break;
            }
            let (cursor, origin) = self.historical_scalar(anchor, byte - 1).ok()?;
            if origin.end != byte || origin.start < range.start {
                return None;
            }
            if let Some(current) = self.visible_scalar(&cursor, origin.len()) {
                return (first.start <= current.start).then_some(first.start..current.end);
            }
            byte = origin.start;
        }
        None
    }

    fn comment_origin(&self, anchor: &AutomergeCommentAnchor) -> Result<Range<usize>, Error> {
        let range = anchor.start as usize..anchor.end as usize;
        if !frontier::valid(&anchor.heads) || range.is_empty() || range.end > MAX_STORY_BYTES {
            return Err(invalid_range());
        }
        if range.len() > MAX_ANCHOR_BYTES {
            return Err(Error::Limit);
        }
        if !ReadDoc::get_missing_deps(&*self.doc, &anchor.heads).is_empty() || range.end > self.doc.length_at(&self.object, &anchor.heads) {
            return Err(invalid_range());
        }
        Ok(range)
    }

    fn historical_scalar(&self, anchor: &AutomergeCommentAnchor, byte: usize) -> Result<(Cursor, Range<usize>), Error> {
        let heads = &anchor.heads;
        let cursor = self.doc.get_cursor(&self.object, byte, Some(heads)).map_err(invalid)?;
        let start = self.doc.get_cursor_position(&self.object, &cursor, Some(heads)).map_err(invalid)?;
        let width = self
            .doc
            .get_at(&self.object, start, heads)
            .map_err(invalid)?
            .and_then(|(value, _)| value.as_str().map(str::len))
            .filter(|width| (1..=4).contains(width))
            .ok_or_else(invalid_range)?;
        Ok((cursor, start..start + width))
    }

    fn visible_scalar(&self, cursor: &Cursor, width: usize) -> Option<Range<usize>> {
        let byte = self.doc.get_cursor_position(&self.object, cursor, None).ok()?;
        // When the original was deleted an ordinary caret cursor may resolve to a neighbour.
        // Compare identities at that position, never their values or offsets.
        let current = self.doc.get_cursor(&self.object, byte, None).ok()?;
        (&current == cursor).then_some(byte..byte + width)
    }
}

fn invalid_range() -> Error {
    Error::Invalid("text comment range")
}

mod frontier {
    use std::borrow::Cow;

    use automerge::ChangeHash;
    use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
    use serde::{Deserialize, Deserializer, Serializer};

    const HASH_BYTES: usize = 32;
    const MAX_ENCODED_BYTES: usize = (super::MAX_HEADS * HASH_BYTES * 4).div_ceil(3);

    pub(super) fn valid(heads: &[ChangeHash]) -> bool {
        !heads.is_empty() && heads.len() <= super::MAX_HEADS && heads.windows(2).all(|pair| pair[0] < pair[1])
    }

    pub(super) fn serialize<S: Serializer>(heads: &[ChangeHash], serializer: S) -> Result<S::Ok, S::Error> {
        let bytes = heads.iter().flat_map(|head| head.0).collect::<Vec<_>>();
        serializer.serialize_str(&STANDARD_NO_PAD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<ChangeHash>, D::Error> {
        let encoded = Cow::<'de, str>::deserialize(deserializer)?;
        if encoded.is_empty() || encoded.len() > MAX_ENCODED_BYTES {
            return Err(serde::de::Error::custom("invalid text comment frontier size"));
        }
        let bytes = STANDARD_NO_PAD.decode(encoded.as_bytes()).map_err(serde::de::Error::custom)?;
        let (hashes, tail) = bytes.as_chunks::<HASH_BYTES>();
        if !tail.is_empty() {
            return Err(serde::de::Error::custom("invalid text comment frontier"));
        }
        let heads: Vec<_> = hashes.iter().copied().map(ChangeHash).collect();
        if !valid(&heads) {
            return Err(serde::de::Error::custom("invalid text comment frontier"));
        }
        Ok(heads)
    }
}
