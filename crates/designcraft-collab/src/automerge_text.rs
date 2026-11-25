//! Automerge replication for the text and formatting of one native story.
//!
//! The room supplies the authenticated actor and delivers a change only after its dependencies.
//! DesignCraft composes the projected story, including any sequence of threaded frames, as usual.

use std::{ops::Range, sync::Arc};

use automerge::{
    ActorId, AutoCommit, Change, Cursor, CursorPosition, ObjId, ObjType, ROOT, ReadDoc, Span, TextEncoding, Value,
    legacy::{Key, ObjectId, OpType},
    transaction::Transactable,
};
use designcraft_doc::{CharRun, ParaFormat, Story, StoryId, Styles};

use crate::{
    Error,
    text::{TextSplice, simple_text},
};

mod format;

use format::{CHARACTER, EMPTY_CHARACTER, FIRST_PARAGRAPH, FOLLOWING_PARAGRAPH, Properties};

const STORY_KEY: &str = "story";
const TEXT_KEY: &str = "text";
const SCHEMA_KEY: &str = "schema";
const SCHEMA: u64 = 1;
const MAX_STORY_BYTES: usize = 4 * 1024 * 1024;
const MAX_STORY_PARAGRAPHS: usize = 16_384;
const MAX_STORY_RUNS: usize = 65_536;
const MAX_HISTORY_OPERATIONS: u64 = 8 * 1024 * 1024;
const MAX_CHANGE_OPERATIONS: usize = 256 * 1024;
const MAX_CHANGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CHECKPOINT_BYTES: usize = 32 * 1024 * 1024;
const MAX_CURSOR_BYTES: usize = 96;

#[derive(Clone, Debug)]
pub struct AutomergeText {
    doc: Arc<AutoCommit>,
    object: ObjId,
    story: StoryId,
}

impl AutomergeText {
    /// Build the same seed independently from the same story. The editing actor must be unique per writer.
    pub fn seed(story: &Story, styles: &Styles, actor: &[u8]) -> Result<Self, Error> {
        validate_story_shape(story)?;
        let mut doc = AutoCommit::new_with_encoding(TextEncoding::Utf8CodeUnit).with_actor(seed_actor(story.id));
        doc.put(ROOT, STORY_KEY, story.id.0).map_err(invalid)?;
        doc.put(ROOT, SCHEMA_KEY, SCHEMA).map_err(invalid)?;
        let object = doc.put_object(ROOT, TEXT_KEY, ObjType::Text).map_err(invalid)?;
        let mut replica = Self { doc: Arc::new(doc), object, story: story.id };
        replica.write_story(story, styles, Some(TextSplice { old: 0..0, new: 0..story.text.len() }))?;
        Arc::make_mut(&mut replica.doc).commit();
        replica.set_actor(actor)?;
        Ok(replica)
    }

    pub fn set_actor(&mut self, actor: &[u8]) -> Result<(), Error> {
        let actor = editing_actor(self.story, actor)?;
        if self.doc.get_actor() != &actor {
            Arc::make_mut(&mut self.doc).set_actor(actor);
        }
        Ok(())
    }

    /// Capture one native command. If projection is incomplete, retain the old replica intact.
    pub fn capture(&mut self, before: &Story, after: &Story, styles: &Styles, hint: Option<TextSplice>) -> Result<Option<Vec<u8>>, Error> {
        if before.id != self.story || after.id != self.story {
            return Err(Error::Invalid("Automerge story identity"));
        }
        validate_story_shape(before)?;
        validate_story_shape(after)?;
        if !self.projection(styles)?.matches(before) {
            return Err(Error::Invalid("Automerge story projection"));
        }

        let mut draft = self.clone();
        let splice = hint.filter(|hint| valid_splice(hint, &before.text, &after.text)).or_else(|| find_splice(&before.text, &after.text));
        draft.write_story(after, styles, splice)?;
        let mut projected = before.clone();
        draft.projection(styles)?.install(&mut projected);
        projected.rev = after.rev;
        if projected != *after {
            return Err(Error::UnsupportedEdit);
        }

        let doc = Arc::make_mut(&mut draft.doc);
        let Some(hash) = doc.commit() else { return Ok(None) };
        let change = doc.get_change_by_hash(&hash).ok_or(Error::Invalid("Automerge change"))?;
        let bytes = change.raw_bytes();
        check_bounds(&change, bytes.len())?;
        check_history(doc)?;
        *self = draft;
        Ok(Some(bytes.into()))
    }

    /// Apply one uncompressed change from the authenticated actor. Duplicate delivery is harmless.
    pub fn apply_changes(&mut self, actor: &[u8], bytes: &[u8], styles: &Styles) -> Result<(), Error> {
        let actor = editing_actor(self.story, actor)?;
        if bytes.len() > MAX_CHANGE_BYTES {
            return Err(Error::Limit);
        }
        // The public binary format puts a one-byte chunk type after the four magic and four checksum bytes.
        // https://automerge.org/automerge-binary-format-spec/ . Accept just a raw change so parsing never inflates remote data.
        if bytes.get(8) != Some(&1) {
            return Err(Error::Invalid("Automerge change chunk"));
        }
        let change = Change::try_from(bytes).map_err(|_| Error::Invalid("Automerge change"))?;
        check_bounds(&change, bytes.len())?;
        if bytes[4..8] != change.hash().0[..4] {
            return Err(Error::Invalid("Automerge change checksum"));
        }
        if change.actor_id() != &actor {
            return Err(Error::Invalid("Automerge change actor"));
        }
        if ReadDoc::get_change_by_hash(&*self.doc, &change.hash()).is_some() {
            return Ok(());
        }
        if change.deps().is_empty() || !ReadDoc::get_missing_deps(&*self.doc, change.deps()).is_empty() {
            return Err(Error::Invalid("Automerge change dependencies"));
        }
        self.validate_operations(&change, styles)?;
        let mut draft = self.clone();
        let doc = Arc::make_mut(&mut draft.doc);
        let hash = change.hash();
        doc.apply_changes([change]).map_err(invalid)?;
        if doc.get_change_by_hash(&hash).is_none() {
            return Err(Error::Invalid("Automerge change dependencies"));
        }
        check_history(doc)?;
        draft.projection(styles)?;
        *self = draft;
        Ok(())
    }

    /// Materialize a story without replacing frame threading, story direction, or native identity.
    pub fn project(&self, story: &mut Story, styles: &Styles) -> Result<(), Error> {
        if story.id != self.story {
            return Err(Error::Invalid("Automerge story identity"));
        }
        validate_story_shape(story)?;
        self.projection(styles)?.install(story);
        Ok(())
    }

    pub fn cursor(&self, byte: usize) -> Result<Vec<u8>, Error> {
        let text = self.doc.text(&self.object).map_err(invalid)?;
        if !text.is_char_boundary(byte) {
            return Err(Error::Invalid("Automerge cursor"));
        }
        let position = if byte == text.len() { CursorPosition::End } else { CursorPosition::Index(byte) };
        Ok(self.doc.get_cursor(&self.object, position, None).map_err(invalid)?.to_bytes())
    }

    pub fn resolve(&self, bytes: &[u8]) -> Result<usize, Error> {
        if bytes.len() > MAX_CURSOR_BYTES {
            return Err(Error::Limit);
        }
        let cursor = Cursor::try_from(bytes).map_err(|_| Error::Invalid("Automerge cursor"))?;
        self.doc.get_cursor_position(&self.object, &cursor, None).map_err(invalid)
    }

    /// A compressed checkpoint for local persistence only. Exchange raw changes with network peers.
    pub fn save(&mut self) -> Result<Vec<u8>, Error> {
        let bytes = Arc::make_mut(&mut self.doc).save();
        if bytes.len() > MAX_CHECKPOINT_BYTES { Err(Error::Limit) } else { Ok(bytes) }
    }

    /// Load a locally trusted checkpoint, and resume writes under the caller's actor.
    pub fn load(story: StoryId, actor: &[u8], bytes: &[u8], styles: &Styles) -> Result<Self, Error> {
        if bytes.len() > MAX_CHECKPOINT_BYTES {
            return Err(Error::Limit);
        }
        let doc = AutoCommit::load(bytes).map_err(invalid)?;
        if doc.text_encoding() != TextEncoding::Utf8CodeUnit
            || doc.get(ROOT, STORY_KEY).map_err(invalid)?.and_then(|(value, _)| value.as_u64()) != Some(story.0)
            || doc.get(ROOT, SCHEMA_KEY).map_err(invalid)?.and_then(|(value, _)| value.as_u64()) != Some(SCHEMA)
        {
            return Err(Error::Invalid("Automerge story schema"));
        }
        let Some((Value::Object(ObjType::Text), object)) = doc.get(ROOT, TEXT_KEY).map_err(invalid)? else {
            return Err(Error::Invalid("Automerge story schema"));
        };
        check_history(&doc)?;
        let mut replica = Self { doc: Arc::new(doc), object, story };
        replica.projection(styles)?;
        replica.set_actor(actor)?;
        Ok(replica)
    }

    fn validate_operations(&self, change: &Change, styles: &Styles) -> Result<(), Error> {
        let ObjId::Id(index, text_actor, _) = &self.object else {
            return Err(Error::Invalid("Automerge story schema"));
        };
        for op in change.decode().operations {
            match (&op.obj, &op.key) {
                (ObjectId::Id(id), Key::Seq(_)) if id.0 == *index && id.1 == *text_actor => match &op.action {
                    OpType::Put(value) if op.insert && value.as_str().is_some_and(|text| text.chars().count() == 1 && simple_text(text)) => {}
                    OpType::Delete if !op.insert => {}
                    OpType::MarkBegin(mark) if op.insert && !mark.expand => {
                        let prefix = if mark.name.starts_with(CHARACTER) { CHARACTER } else { FOLLOWING_PARAGRAPH };
                        format::validate_property(&mark.name, Some(&mark.value), prefix, styles)?;
                    }
                    OpType::MarkEnd(false) if op.insert => {}
                    _ => return Err(Error::Invalid("Automerge text operation")),
                },
                (ObjectId::Root, Key::Map(key)) if !op.insert => {
                    let prefix = if key.starts_with(FIRST_PARAGRAPH) { FIRST_PARAGRAPH } else { EMPTY_CHARACTER };
                    let value = match &op.action {
                        OpType::Put(value) if !value.is_null() => Some(value),
                        OpType::Delete => None,
                        _ => return Err(Error::Invalid("Automerge formatting operation")),
                    };
                    format::validate_property(key, value, prefix, styles)?;
                }
                _ => return Err(Error::Invalid("Automerge story operation")),
            }
        }
        Ok(())
    }

    fn write_story(&mut self, story: &Story, styles: &Styles, splice: Option<TextSplice>) -> Result<(), Error> {
        if let Some(splice) = splice
            && (!splice.old.is_empty() || !splice.new.is_empty())
        {
            let deleted = isize::try_from(splice.old.len()).map_err(|_| Error::Limit)?;
            Arc::make_mut(&mut self.doc).splice_text(&self.object, splice.old.start, deleted, &story.text[splice.new]).map_err(invalid)?;
        }
        let current = self.projection(styles)?;
        let doc = Arc::make_mut(&mut self.doc);
        if let ([old], [new]) = (current.chars.as_slice(), story.chars.as_slice())
            && story.text.is_empty()
            && old.format != new.format
        {
            Properties::characters(&new.format, styles)?.write_root(&Properties::characters(&old.format, styles)?, doc, EMPTY_CHARACTER)?;
        } else if !story.text.is_empty() {
            write_character_marks(doc, &self.object, styles, &current.chars, &story.chars)?;
        }

        if current.paras[0] != story.paras[0] {
            Properties::paragraph(&story.paras[0], styles)?.write_root(&Properties::paragraph(&current.paras[0], styles)?, doc, FIRST_PARAGRAPH)?;
        }
        let newlines = story.text.match_indices('\n').map(|(index, _)| index);
        for (offset, (old, new)) in newlines.zip(current.paras[1..].iter().zip(&story.paras[1..])) {
            if old != new {
                Properties::paragraph(new, styles)?.write_marks(
                    &Properties::paragraph(old, styles)?,
                    doc,
                    &self.object,
                    FOLLOWING_PARAGRAPH,
                    offset,
                    offset + 1,
                )?;
            }
        }
        Ok(())
    }

    fn projection(&self, styles: &Styles) -> Result<Projection, Error> {
        if self.doc.length(&self.object) > MAX_STORY_BYTES {
            return Err(Error::Limit);
        }
        let first = Properties::read_root(&self.doc, FIRST_PARAGRAPH)?.paragraph_format(styles)?;
        let mut projection = Projection { text: String::new(), chars: Vec::new(), paras: vec![first] };
        for span in self.doc.spans(&self.object).map_err(invalid)? {
            let Span::Text { text, marks } = span else { return Err(Error::Invalid("Automerge story text")) };
            if !simple_text(&text) {
                return Err(Error::UnsupportedEdit);
            }
            let (characters, next) = Properties::read_marks(marks.as_deref())?;
            if !next.is_empty() && !text.bytes().all(|byte| byte == b'\n') {
                return Err(Error::Invalid("Automerge paragraph mark"));
            }
            let format = characters.character_format(styles)?;
            projection.push_run(text.len(), format);
            let breaks = text.bytes().filter(|&byte| byte == b'\n').count();
            if projection.chars.len() > MAX_STORY_RUNS || projection.paras.len() + breaks > MAX_STORY_PARAGRAPHS {
                return Err(Error::Limit);
            }
            if breaks != 0 {
                let paragraph = next.paragraph_format(styles)?;
                projection.paras.extend(std::iter::repeat_n(paragraph, breaks));
            }
            projection.text.push_str(&text);
        }
        if projection.text.is_empty() {
            let format = Properties::read_root(&self.doc, EMPTY_CHARACTER)?.character_format(styles)?;
            projection.chars.push(CharRun { len: 0, format });
        }
        Ok(projection)
    }
}

struct Projection {
    text: String,
    chars: Vec<CharRun>,
    paras: Vec<ParaFormat>,
}

impl Projection {
    fn push_run(&mut self, len: usize, format: designcraft_doc::CharFormat) {
        if let Some(last) = self.chars.last_mut().filter(|last| last.format == format) {
            last.len += len;
        } else if len != 0 {
            self.chars.push(CharRun { len, format });
        }
    }

    fn matches(&self, story: &Story) -> bool {
        self.text == story.text && self.chars == story.chars && self.paras == story.paras
    }

    fn install(self, story: &mut Story) {
        if !self.matches(story) {
            story.text = self.text;
            story.chars = self.chars;
            story.paras = self.paras;
            story.rev = story.rev.wrapping_add(1);
        }
    }
}

/// The first migration supports rich text in ordinary stories, including threaded frames.
pub fn validate_story_shape(story: &Story) -> Result<(), Error> {
    if story.text.len() > MAX_STORY_BYTES || story.chars.len() > MAX_STORY_RUNS || story.paras.len() > MAX_STORY_PARAGRAPHS {
        return Err(Error::Limit);
    }
    if story.link.is_some()
        || story.file_link.is_some()
        || !story.tables.is_empty()
        || !story.notes.is_empty()
        || !story.endnotes.is_empty()
        || !story.anchors.is_empty()
        || !story.xrefs.is_empty()
        || !story.index_refs.is_empty()
        || !story.objects.is_empty()
        || !story.editorial.is_empty()
        || story.paras.iter().any(|p| p.table.is_some())
        || !simple_text(&story.text)
    {
        return Err(Error::UnsupportedEdit);
    }
    if story.paras.len() != story.text.bytes().filter(|&byte| byte == b'\n').count() + 1 || story.chars.is_empty() {
        return Err(Error::Invalid("Automerge story shape"));
    }
    if story.text.is_empty() && story.chars.len() != 1 {
        return Err(Error::Invalid("Automerge character runs"));
    }
    let mut end: usize = 0;
    for run in &story.chars {
        end = end.checked_add(run.len).ok_or(Error::Limit)?;
        if !story.text.is_char_boundary(end) || (run.len == 0 && !story.text.is_empty()) {
            return Err(Error::Invalid("Automerge character runs"));
        }
    }
    if end == story.text.len() { Ok(()) } else { Err(Error::Invalid("Automerge character runs")) }
}

fn write_character_marks(doc: &mut AutoCommit, object: &ObjId, styles: &Styles, old: &[CharRun], new: &[CharRun]) -> Result<(), Error> {
    let mut old = old.iter();
    let mut new = new.iter();
    let mut left = old.next().map(|run| (run, run.len));
    let mut right = new.next().map(|run| (run, run.len));
    let mut start = 0;
    while let (Some((before, remaining_before)), Some((after, remaining_after))) = (left, right) {
        let len = remaining_before.min(remaining_after);
        let end = start + len;
        if before.format != after.format {
            Properties::characters(&after.format, styles)?.write_marks(
                &Properties::characters(&before.format, styles)?,
                doc,
                object,
                CHARACTER,
                start,
                end,
            )?;
        }
        start = end;
        left = if len == remaining_before { old.next().map(|run| (run, run.len)) } else { Some((before, remaining_before - len)) };
        right = if len == remaining_after { new.next().map(|run| (run, run.len)) } else { Some((after, remaining_after - len)) };
    }
    if left.is_some() || right.is_some() {
        return Err(Error::Invalid("Automerge character projection"));
    }
    Ok(())
}

fn valid_splice(hint: &TextSplice, before: &str, after: &str) -> bool {
    hint.old.start == hint.new.start
        && before.get(hint.old.clone()).is_some()
        && after.get(hint.new.clone()).is_some()
        && before.get(..hint.old.start) == after.get(..hint.new.start)
        && before.get(hint.old.end..) == after.get(hint.new.end..)
}

fn find_splice(before: &str, after: &str) -> Option<TextSplice> {
    if before == after {
        return None;
    }
    let same_bytes = |a: char, b: char| (a == b).then(|| a.len_utf8());
    let start = before.chars().zip(after.chars()).map_while(|(a, b)| same_bytes(a, b)).sum::<usize>();
    let suffix = before[start..].chars().rev().zip(after[start..].chars().rev()).map_while(|(a, b)| same_bytes(a, b)).sum::<usize>();
    let range = |text: &str| -> Range<usize> { start..text.len() - suffix };
    Some(TextSplice { old: range(before), new: range(after) })
}

fn seed_actor(story: StoryId) -> ActorId {
    let mut name = b"collate-story-v1:".to_vec();
    name.extend(story.0.to_be_bytes());
    ActorId::from(name)
}

fn editing_actor(story: StoryId, actor: &[u8]) -> Result<ActorId, Error> {
    if actor.is_empty() || actor.len() > 64 || actor == seed_actor(story).to_bytes() {
        return Err(Error::Invalid("Automerge story actor"));
    }
    Ok(ActorId::from(actor))
}

fn check_bounds(change: &Change, bytes: usize) -> Result<(), Error> {
    if change.is_empty() || change.len() > MAX_CHANGE_OPERATIONS || bytes > MAX_CHANGE_BYTES || change.deps().len() > 64 {
        return Err(Error::Limit);
    }
    Ok(())
}

fn check_history(doc: &AutoCommit) -> Result<(), Error> {
    if doc.stats().num_ops > MAX_HISTORY_OPERATIONS { Err(Error::Limit) } else { Ok(()) }
}

fn invalid(_: automerge::AutomergeError) -> Error {
    Error::Invalid("Automerge story data")
}

#[cfg(test)]
mod tests;
