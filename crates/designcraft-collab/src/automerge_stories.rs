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
    automerge_text::{AutomergeText, append_inserted_text},
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
}

impl AutomergeStories {
    pub(crate) fn get(&self, id: StoryId) -> Option<&AutomergeText> {
        self.stories.get(&id).map(Arc::as_ref)
    }

    pub(crate) fn remove(&mut self, id: StoryId) {
        self.stories.remove(&id);
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
        let mut native = before.clone();
        let mut staged = self.clone();
        let mut transaction = StoryTransaction { changes: Vec::new() };
        for (&id, current) in &after.stories {
            let previous = before.stories.get(&id).ok_or(Error::UnsupportedEdit)?;
            if Arc::ptr_eq(previous, current) || previous == current {
                continue;
            }
            check_native_ranges(before, id)?;
            let hint = splice.as_ref().filter(|(story, _)| *story == id).map(|(_, hint)| hint.clone());
            let story = staged.open(previous, &before.styles, actor)?;
            story.set_actor(actor)?;
            if let Some(bytes) = story.capture(previous, current, &before.styles, hint)? {
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
            let story = staged.open(original, &document.styles, actor)?;
            let mut output = original.clone();
            story.project(&mut output, &document.styles)?;
            if output.text != original.text || output.chars != original.chars || output.paras != original.paras {
                return Err(Error::Invalid("story projection"));
            }
            story.apply_changes(actor, &change.bytes, &document.styles)?;
            story.project(&mut output, &document.styles)?;
            projected.push((change.story, Arc::new(output)));
        }
        document.stories.extend(projected);
        *self = staged;
        Ok(())
    }

    fn open(&mut self, original: &Story, styles: &Styles, actor: &[u8; 32]) -> Result<&mut AutomergeText, Error> {
        use std::collections::btree_map::Entry;

        let story = match self.stories.entry(original.id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Arc::new(AutomergeText::seed(original, styles, actor)?)),
        };
        Ok(Arc::make_mut(story))
    }
}

// Native links and data-merge placeholders still use UTF-8 offsets. A remote splice
// cannot leave these ranges pointing at different content. They need CRDT anchors first.
fn check_native_ranges(document: &Document, id: StoryId) -> Result<(), Error> {
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
        if self.changes.is_empty() {
            return Err(Error::Invalid("empty story transaction"));
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
