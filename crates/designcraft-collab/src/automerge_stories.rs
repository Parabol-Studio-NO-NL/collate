//! Bridge between Automerge story changes and the native document.
//!
//! The native seed and the ordered structural operations establish story identities.
//! Only changed stories are opened as Automerge documents. Layout and artwork stay native.

use std::{collections::BTreeMap, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use designcraft_doc::{Document, HyperlinkSource, PlaceholderAnchor, Story, StoryId, Styles};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    Error,
    automerge_text::{AutomergeText, append_inserted_text, reversible::ReversibleText},
    text::TextSplice,
};

const MAX_STORIES_PER_EDIT: usize = 64;
const MAX_TRANSACTION_BYTES: usize = 2 * 1024 * 1024;

/// A set of Automerge changes from one authenticated author. The caller supplies that
/// author from its transport, so this payload cannot claim an author of its own.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoryTransaction {
    pub changes: Vec<StoryChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reversible_sequence: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoryChange {
    pub story: StoryId,
    #[serde(serialize_with = "serialize_change", deserialize_with = "deserialize_change")]
    pub bytes: Vec<u8>,
}

/// Per-story Automerge state for a single room. The caller retains the native seed
/// and journals accepted transactions before acknowledging them to their senders.
#[derive(Clone, Default)]
pub struct AutomergeStories {
    stories: BTreeMap<StoryId, Arc<AutomergeText>>,
    reversible: BTreeMap<StoryId, Arc<ReversibleText>>,
}

impl AutomergeStories {
    pub(crate) fn get(&self, id: StoryId) -> Option<&AutomergeText> {
        self.stories.get(&id).map(Arc::as_ref)
    }

    pub(crate) fn reversible(&self, id: StoryId) -> Option<&ReversibleText> {
        self.reversible.get(&id).map(Arc::as_ref)
    }

    pub(crate) fn contains(&self, id: StoryId) -> bool {
        self.stories.contains_key(&id) || self.reversible.contains_key(&id)
    }

    pub(crate) fn remove(&mut self, id: StoryId) {
        self.stories.remove(&id);
        self.reversible.remove(&id);
    }

    /// Record a native text edit locally and return the changes to send. A structural
    /// edit must take the separate structural path. A failed edit leaves this replica intact.
    pub fn capture(
        &mut self,
        before: &Document,
        after: &Document,
        actor: &[u8; 32],
        splice: Option<(StoryId, TextSplice)>,
    ) -> Result<Option<StoryTransaction>, Error> {
        self.capture_version(before, after, actor, None, splice)
    }

    pub(crate) fn capture_reversible(
        &mut self,
        before: &Document,
        after: &Document,
        actor: &[u8; 32],
        sequence: u64,
        splice: Option<(StoryId, TextSplice)>,
    ) -> Result<Option<StoryTransaction>, Error> {
        // A pre-existing schema-1 story keeps its original history and anchors.
        // A mixed-schema edit is rejected by either version's exclusive open path.
        let has_old_story = after
            .stories
            .iter()
            .any(|(id, story)| self.stories.contains_key(id) && before.stories.get(id).is_some_and(|old| !Arc::ptr_eq(old, story) && old != story));
        let version = if has_old_story { None } else { Some(sequence) };
        self.capture_version(before, after, actor, version, splice)
    }

    fn capture_version(
        &mut self,
        before: &Document,
        after: &Document,
        actor: &[u8; 32],
        reversible_sequence: Option<u64>,
        splice: Option<(StoryId, TextSplice)>,
    ) -> Result<Option<StoryTransaction>, Error> {
        let mut native = before.clone();
        let mut staged = self.clone();
        let mut transaction = StoryTransaction { changes: Vec::new(), reversible_sequence };
        for (&id, current) in &after.stories {
            let previous = before.stories.get(&id).ok_or(Error::UnsupportedEdit)?;
            if Arc::ptr_eq(previous, current) || previous == current {
                continue;
            }
            check_native_ranges(before, id)?;
            let hint = splice.as_ref().filter(|(story, _)| *story == id).map(|(_, hint)| hint.clone());
            let bytes = if let Some(sequence) = reversible_sequence {
                staged.open_reversible(previous, &before.styles)?.capture(previous, current, &before.styles, actor, sequence, hint)?
            } else {
                let story = staged.open(previous, &before.styles, actor)?;
                story.set_actor(actor)?;
                story.capture(previous, current, &before.styles, hint)?
            };
            if let Some(bytes) = bytes {
                if transaction.changes.len() == MAX_STORIES_PER_EDIT {
                    return Err(Error::Limit);
                }
                transaction.changes.push(StoryChange { story: id, bytes });
            }
            native.stories.insert(id, current.clone());
        }
        if native != *after {
            return Err(Error::UnsupportedEdit);
        }
        if transaction.changes.is_empty() {
            return Ok(None);
        }
        transaction.validate()?;
        *self = staged;
        Ok(Some(transaction))
    }

    /// Merge the transaction, which may have been created concurrently on another
    /// peer. Either all affected native stories are updated or none are.
    pub fn apply(&mut self, document: &mut Document, actor: &[u8; 32], transaction: &StoryTransaction) -> Result<(), Error> {
        transaction.validate()?;
        let mut staged = self.clone();
        let mut projected = Vec::with_capacity(transaction.changes.len());
        for change in &transaction.changes {
            let original = document.story(change.story).ok_or(Error::UnknownResource)?;
            check_native_ranges(document, change.story)?;
            let mut output = original.clone();
            if let Some(sequence) = transaction.reversible_sequence {
                let story = staged.open_reversible(original, &document.styles)?;
                story.project(&mut output, &document.styles)?;
                if output != *original {
                    return Err(Error::Invalid("story projection"));
                }
                story.apply(actor, sequence, &change.bytes, &document.styles)?;
                story.project(&mut output, &document.styles)?;
            } else {
                let story = staged.open(original, &document.styles, actor)?;
                story.project(&mut output, &document.styles)?;
                if output != *original {
                    return Err(Error::Invalid("story projection"));
                }
                story.apply_changes(actor, &change.bytes, &document.styles)?;
                story.project(&mut output, &document.styles)?;
            }
            projected.push((change.story, Arc::new(output)));
        }
        document.stories.extend(projected);
        *self = staged;
        Ok(())
    }

    pub(crate) fn open(&mut self, original: &Story, styles: &Styles, actor: &[u8; 32]) -> Result<&mut AutomergeText, Error> {
        use std::collections::btree_map::Entry;

        if self.reversible.contains_key(&original.id) {
            return Err(Error::UnsupportedEdit);
        }
        let story = match self.stories.entry(original.id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Arc::new(AutomergeText::seed(original, styles, actor)?)),
        };
        Ok(Arc::make_mut(story))
    }

    pub(crate) fn open_reversible(&mut self, original: &Story, styles: &Styles) -> Result<&mut ReversibleText, Error> {
        use std::collections::btree_map::Entry;

        if self.stories.contains_key(&original.id) {
            return Err(Error::UnsupportedEdit);
        }
        let story = match self.reversible.entry(original.id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Arc::new(ReversibleText::seed(original, styles)?)),
        };
        Ok(Arc::make_mut(story))
    }

    pub(crate) fn toggle(&mut self, story: &mut Story, styles: &Styles, actor: &str, sequence: u64, active: bool) -> Result<(), Error> {
        Arc::make_mut(self.reversible.get_mut(&story.id).ok_or(Error::UnknownResource)?).toggle(story, styles, actor, sequence, active)
    }
}

// Native links and data-merge placeholders still use UTF-8 offsets. A remote splice
// cannot leave these ranges pointing at different content. They need CRDT anchors first.
pub(crate) fn check_native_ranges(document: &Document, id: StoryId) -> Result<(), Error> {
    if document.hyperlinks.iter().any(|link| matches!(link.source, HyperlinkSource::Text { story, .. } if story == id))
        || document
            .data_merge
            .placeholders
            .iter()
            .any(|placeholder| matches!(placeholder.anchor, PlaceholderAnchor::Text { story, .. } if story == id))
    {
        return Err(Error::UnsupportedEdit);
    }
    Ok(())
}

fn serialize_change<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&STANDARD_NO_PAD.encode(bytes))
}

fn deserialize_change<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let encoded = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
    if encoded.len() > (MAX_TRANSACTION_BYTES * 4).div_ceil(3) {
        return Err(serde::de::Error::custom(Error::Limit));
    }
    STANDARD_NO_PAD.decode(encoded.as_bytes()).map_err(serde::de::Error::custom)
}

impl StoryTransaction {
    /// The authenticated proposal sequence for reversible edits. Older commits omit it.
    pub fn reversible_sequence(&self) -> Option<u64> {
        self.reversible_sequence
    }

    /// Recover inserted text from a locally submitted change that the host declined.
    pub fn inserted_text(&self) -> Result<String, Error> {
        self.validate()?;
        let mut text = String::new();
        for change in &self.changes {
            append_inserted_text(&change.bytes, &mut text)?;
        }
        Ok(text)
    }

    fn validate(&self) -> Result<(), Error> {
        if self.changes.is_empty() || self.reversible_sequence == Some(0) {
            return Err(Error::Invalid("story transaction"));
        }
        if self.changes.len() > MAX_STORIES_PER_EDIT {
            return Err(Error::Limit);
        }
        let mut remaining = MAX_TRANSACTION_BYTES;
        let mut previous = None;
        for change in &self.changes {
            if change.bytes.is_empty() || previous.is_some_and(|id| id >= change.story) {
                return Err(Error::Invalid("story transaction"));
            }
            remaining = remaining.checked_sub(change.bytes.len()).ok_or(Error::Limit)?;
            previous = Some(change.story);
        }
        Ok(())
    }
}
