//! Author undo for schema-2 stories. Automerge orders permanent character identities;
//! inactive births and active delete effects only hide them from the native projection.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    sync::{Arc, OnceLock},
};

use automerge::{
    ActorId, AutoCommit, Change, Cursor, CursorPosition, ObjId, ObjType, ROOT, ReadDoc, ScalarValueRef, TextEncoding, ValueRef,
    legacy::{Key, ObjectId, OpType},
    transaction::Transactable,
};
use designcraft_doc::{CharFormat, CharRun, Story, StoryId, Styles};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Error,
    text::{MAX_EDIT_ATOMS, TextSplice, simple_text},
};

use super::{Projection, check_bounds, check_history, find_splice, format::Properties, invalid, parse_change, valid_splice, validate_story_shape};

const BATCH_KEY: &str = "story-effect-v2";
const MAX_EFFECT_BYTES: usize = 1024 * 1024;
const MAX_EFFECT_TARGETS: usize = 512 * 1024;
const MAX_PHYSICAL_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct AtomId {
    actor: [u8; 32],
    counter: u64,
}

impl AtomId {
    fn new(id: ObjId) -> Result<Self, Error> {
        let ObjId::Id(counter, actor, _) = id else { return Err(invalid_effect()) };
        let actor = actor.to_bytes().try_into().map_err(|_| invalid_effect())?;
        Ok(Self { actor, counter })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct AtomRun {
    first: AtomId,
    count: u32,
}

impl AtomRun {
    fn push(runs: &mut Vec<Self>, id: AtomId) {
        if let Some(last) = runs.last_mut()
            && id.actor == last.first.actor
            && last.first.counter.checked_add(u64::from(last.count)) == Some(id.counter)
        {
            last.count += 1;
        } else {
            runs.push(Self { first: id, count: 1 });
        }
    }

    fn ids(&self) -> impl Iterator<Item = AtomId> {
        (0..u64::from(self.count)).filter_map(|delta| Some(AtomId { counter: self.first.counter.checked_add(delta)?, ..self.first }))
    }

    fn valid(&self) -> bool {
        self.count != 0 && self.first.counter != 0 && self.first.counter.checked_add(u64::from(self.count - 1)).is_some()
    }
}

/// A compact set of original character identities, in the original selection order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReversibleCommentAnchor {
    runs: Vec<AtomRun>,
}

type Writes = BTreeMap<String, Option<String>>;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Effects {
    sequence: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deletes: Vec<AtomRun>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    characters: Vec<PropertyEffect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paragraphs: Vec<PropertyEffect>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    first: Writes,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    empty: Writes,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PropertyEffect {
    atoms: Vec<AtomRun>,
    writes: Writes,
}

#[derive(Clone)]
struct Record {
    effects: Effects,
    births: Vec<AtomRun>,
    rank: u64,
    hash: automerge::ChangeHash,
    active: bool,
}

#[derive(Clone)]
struct Atom {
    id: AtomId,
    scalar: char,
    position: usize,
}

struct View {
    projection: Projection,
    visible: Arc<Vec<Atom>>,
}

#[derive(Clone)]
pub(crate) struct ReversibleText {
    doc: Arc<AutoCommit>,
    object: ObjId,
    story: StoryId,
    records: BTreeMap<(String, u64), Arc<Record>>,
    initial: Arc<Effects>,
    visible: Arc<OnceLock<Arc<Vec<Atom>>>>,
}

impl ReversibleText {
    pub(crate) fn seed(story: &Story, styles: &Styles) -> Result<Self, Error> {
        validate_story_shape(story)?;
        let seed_format = serde_json::to_vec(&(&story.chars, &story.paras)).map_err(|_| invalid_effect())?;
        let format_hash = Sha256::digest(seed_format);
        let mut hash = Sha256::new();
        hash.update(b"Collate reversible story\0");
        hash.update(story.id.0.to_be_bytes());
        hash.update(format_hash);
        hash.update(story.text.as_bytes());
        let seed_actor = ActorId::from(hash.finalize().to_vec());
        let mut doc = AutoCommit::new_with_encoding(TextEncoding::Utf8CodeUnit).with_actor(seed_actor);
        let object = doc.put_object(ROOT, "text", ObjType::Text).map_err(invalid)?;
        doc.put(ROOT, "format", format_hash.to_vec()).map_err(invalid)?;
        doc.splice_text(&object, 0, 0, &story.text).map_err(invalid)?;
        doc.commit();
        let mut replica =
            Self { doc: Arc::new(doc), object, story: story.id, records: BTreeMap::new(), initial: Arc::default(), visible: Arc::default() };
        let initial = replica.view(styles, &[])?;
        replica.initial = Arc::new(format_effects(&initial, story, styles)?);
        Ok(replica)
    }

    pub(crate) fn capture(
        &mut self,
        before: &Story,
        after: &Story,
        styles: &Styles,
        actor: &[u8; 32],
        sequence: u64,
        hint: Option<TextSplice>,
    ) -> Result<Option<Vec<u8>>, Error> {
        if before.id != self.story || after.id != self.story || sequence == 0 {
            return Err(invalid_effect());
        }
        validate_story_shape(before)?;
        validate_story_shape(after)?;
        let view = self.view(styles, &[])?;
        if !view.projection.matches(before) {
            return Err(Error::Invalid("story projection"));
        }
        let mut amended = self.clone();
        amended.visible = Arc::default();
        let actor = ActorId::from(actor.as_slice());
        let key = (actor.to_string(), sequence);
        if amended.records.contains_key(&key) || amended.object_actor() == Some(&actor) {
            return Err(Error::DuplicateId);
        }
        Arc::make_mut(&mut amended.doc).set_actor(actor);
        let splice = hint.filter(|hint| valid_splice(hint, &before.text, &after.text)).or_else(|| find_splice(&before.text, &after.text));
        let mut deletes = Vec::new();
        if let Some(splice) = splice {
            let start = logical_index(&view.visible, splice.old.start).ok_or_else(invalid_effect)?;
            let end = logical_index(&view.visible, splice.old.end).ok_or_else(invalid_effect)?;
            for atom in &view.visible[start..end] {
                AtomRun::push(&mut deletes, atom.id);
            }
            let physical = start.checked_sub(1).map_or(0, |index| {
                let atom = &view.visible[index];
                atom.position + atom.scalar.len_utf8()
            });
            Arc::make_mut(&mut amended.doc).splice_text(&amended.object, physical, 0, &after.text[splice.new]).map_err(invalid)?;
        }
        let view = amended.view(styles, &deletes)?;
        let mut effects = format_effects(&view, after, styles)?;
        effects.sequence = sequence;
        effects.deletes = deletes;
        let doc = Arc::make_mut(&mut amended.doc);
        if doc.pending_ops() == 0 && effects.is_empty() {
            return if before == after || only_revision_changed(before, after) { Ok(None) } else { Err(Error::UnsupportedEdit) };
        }
        let payload = encode_effects(&effects)?;
        doc.put(ROOT, batch_key(&key.0, sequence), payload).map_err(invalid)?;
        let hash = doc.commit().ok_or_else(invalid_effect)?;
        let change = doc.get_change_by_hash(&hash).ok_or_else(invalid_effect)?;
        let bytes = change.raw_bytes().to_vec();
        // The same checks govern locally produced or received changes.
        let record = self.validate_change(&change, styles, sequence)?;
        check_bounds(&change, bytes.len())?;
        check_history(doc)?;
        amended.records.insert(key, Arc::new(record));
        amended.visible = Arc::default();
        let mut projected = before.clone();
        amended.project(&mut projected, styles)?;
        projected.rev = after.rev;
        if projected != *after {
            return Err(Error::UnsupportedEdit);
        }
        *self = amended;
        Ok(Some(bytes))
    }

    pub(crate) fn apply(&mut self, actor: &[u8; 32], sequence: u64, bytes: &[u8], styles: &Styles) -> Result<(), Error> {
        let change = parse_change(bytes)?;
        if bytes[4..8] != change.hash().0[..4] || change.actor_id().to_bytes() != actor {
            return Err(invalid_effect());
        }
        let key = (change.actor_id().to_string(), sequence);
        if ReadDoc::get_change_by_hash(&*self.doc, &change.hash()).is_some() {
            return if self.records.get(&key).is_some_and(|record| record.hash == change.hash()) { Ok(()) } else { Err(invalid_effect()) };
        }
        if self.records.contains_key(&key) {
            return Err(Error::DuplicateId);
        }
        let record = self.validate_change(&change, styles, sequence)?;
        let mut draft = self.clone();
        draft.visible = Arc::default();
        let doc = Arc::make_mut(&mut draft.doc);
        let hash = change.hash();
        doc.apply_changes([change]).map_err(invalid)?;
        if doc.get_change_by_hash(&hash).is_none() {
            return Err(Error::Invalid("Automerge change dependencies"));
        }
        check_history(doc)?;
        draft.records.insert(key, Arc::new(record));
        draft.view(styles, &[])?;
        *self = draft;
        Ok(())
    }

    fn validate_change(&self, change: &Change, styles: &Styles, sequence: u64) -> Result<Record, Error> {
        if sequence == 0
            || self.object_actor() == Some(change.actor_id())
            || change.deps().is_empty()
            || !ReadDoc::get_missing_deps(&*self.doc, change.deps()).is_empty()
        {
            return Err(invalid_effect());
        }
        let ObjId::Id(object_counter, object_actor, _) = &self.object else { return Err(invalid_effect()) };
        let actor = change.actor_id().to_bytes().try_into().map_err(|_| invalid_effect())?;
        let expected_key = batch_key(&change.actor_id().to_string(), sequence);
        let mut effects = None;
        let mut births = Vec::new();
        let counter = change.start_op().get();
        let mut new_atoms = BTreeMap::new();
        for (index, op) in change.decode().operations.into_iter().enumerate() {
            match (op.obj, op.key, op.action) {
                (ObjectId::Root, Key::Map(key), OpType::Put(value))
                    if key == expected_key && !op.insert && effects.is_none() && op.pred.is_empty() =>
                {
                    let bytes = value.as_bytes().ok_or_else(invalid_effect)?;
                    if bytes.len() > MAX_EFFECT_BYTES {
                        return Err(Error::Limit);
                    }
                    effects = Some(serde_json::from_slice::<Effects>(bytes).map_err(|_| invalid_effect())?);
                }
                (ObjectId::Id(id), Key::Seq(_), OpType::Put(value))
                    if id.0 == *object_counter && id.1 == *object_actor && op.insert && op.pred.is_empty() =>
                {
                    let text = value.as_str().filter(|text| simple_text(text)).ok_or_else(invalid_effect)?;
                    let mut scalars = text.chars();
                    let scalar = scalars.next().filter(|_| scalars.next().is_none()).ok_or_else(invalid_effect)?;
                    let id = AtomId { actor, counter: counter.checked_add(index as u64).ok_or(Error::Limit)? };
                    new_atoms.insert(id, scalar);
                    AtomRun::push(&mut births, id);
                }
                _ => return Err(invalid_effect()),
            }
        }
        let effects = effects.filter(|effects| effects.sequence == sequence).ok_or_else(invalid_effect)?;
        if births.is_empty() && effects.is_empty() {
            return Err(invalid_effect());
        }
        let targets = effects.target_count()?;
        if targets > MAX_EFFECT_TARGETS {
            return Err(Error::Limit);
        }
        if targets > 0 {
            let old = self.physical(Some(change.deps()))?.into_iter().map(|atom| (atom.id, atom.scalar)).collect::<BTreeMap<_, _>>();
            effects.validate_targets(&old, &new_atoms)?;
        }
        let mut scratch = Properties::default();
        scratch.apply_writes(&effects.first, false, styles)?;
        Properties::default().apply_writes(&effects.empty, true, styles)?;
        for effect in &effects.characters {
            Properties::default().apply_writes(&effect.writes, true, styles)?;
        }
        for effect in &effects.paragraphs {
            Properties::default().apply_writes(&effect.writes, false, styles)?;
        }
        Ok(Record { effects, births, rank: counter, hash: change.hash(), active: true })
    }

    fn object_actor(&self) -> Option<&ActorId> {
        match &self.object {
            ObjId::Id(_, actor, _) => Some(actor),
            ObjId::Root => None,
        }
    }

    pub(crate) fn can_toggle(&self, actor: &str, sequence: u64, active: bool) -> bool {
        self.records.get(&(actor.to_owned(), sequence)).is_some_and(|record| record.active != active)
    }

    pub(crate) fn toggle(&mut self, story: &mut Story, styles: &Styles, actor: &str, sequence: u64, active: bool) -> Result<(), Error> {
        if !self.can_toggle(actor, sequence, active) {
            return Err(Error::Invalid("text history state"));
        }
        let mut draft = self.clone();
        draft.visible = Arc::default();
        let record = draft.records.get_mut(&(actor.to_owned(), sequence)).ok_or_else(invalid_effect)?;
        Arc::make_mut(record).active = active;
        draft.project(story, styles)?;
        *self = draft;
        Ok(())
    }

    pub(crate) fn project(&self, story: &mut Story, styles: &Styles) -> Result<(), Error> {
        if story.id != self.story {
            return Err(invalid_effect());
        }
        self.view(styles, &[])?.projection.install(story);
        Ok(())
    }

    pub(crate) fn cursor(&self, byte: usize) -> Result<Vec<u8>, Error> {
        let atoms = self.visible(&[])?;
        let index = logical_index(&atoms, byte).ok_or_else(invalid_effect)?;
        // Keep the caret behind the editor's preceding character when another peer
        // inserts into the same gap. Its original identity remains available if hidden.
        let position = index.checked_sub(1).map_or(CursorPosition::Start, |previous| CursorPosition::Index(atoms[previous].position));
        Ok(self.doc.get_cursor(&self.object, position, None).map_err(invalid)?.to_bytes())
    }

    pub(crate) fn resolve(&self, bytes: &[u8]) -> Result<usize, Error> {
        if bytes.len() > super::MAX_CURSOR_BYTES {
            return Err(Error::Limit);
        }
        let cursor = Cursor::try_from(bytes).map_err(|_| invalid_effect())?;
        if cursor == Cursor::Start {
            return Ok(0);
        }
        let physical = self.doc.get_cursor_position(&self.object, &cursor, None).map_err(invalid)?;
        Ok(self.visible(&[])?.iter().take_while(|atom| atom.position <= physical).map(|atom| atom.scalar.len_utf8()).sum())
    }

    pub(crate) fn comment_anchor(&self, range: Range<usize>) -> Result<ReversibleCommentAnchor, Error> {
        let visible = self.visible(&[])?;
        let start = logical_index(&visible, range.start).ok_or_else(invalid_effect)?;
        let end = logical_index(&visible, range.end).ok_or_else(invalid_effect)?;
        let count = end.checked_sub(start).filter(|count| *count > 0).ok_or_else(invalid_effect)?;
        if count > MAX_EDIT_ATOMS {
            return Err(Error::Limit);
        }
        let mut runs = Vec::new();
        for atom in &visible[start..end] {
            AtomRun::push(&mut runs, atom.id);
        }
        Ok(ReversibleCommentAnchor { runs })
    }

    pub(crate) fn validate_comment_anchor(&self, anchor: &ReversibleCommentAnchor) -> Result<(), Error> {
        let ids = comment_ids(anchor)?;
        let mut physical = self.physical(None)?.into_iter();
        if !ids.into_iter().all(|id| physical.any(|atom| atom.id == id)) {
            return Err(invalid_effect());
        }
        Ok(())
    }

    pub(crate) fn comment_range(&self, anchor: &ReversibleCommentAnchor) -> Option<Range<usize>> {
        let ids: BTreeSet<_> = comment_ids(anchor).ok()?.into_iter().collect();
        let mut position = 0;
        let mut range: Option<Range<usize>> = None;
        for atom in self.visible(&[]).ok()?.iter() {
            let after = position + atom.scalar.len_utf8();
            if ids.contains(&atom.id) {
                if let Some(range) = &mut range { range.end = after } else { range = Some(position..after) }
            }
            position = after;
        }
        range
    }

    fn physical(&self, heads: Option<&[automerge::ChangeHash]>) -> Result<Vec<Atom>, Error> {
        let length = heads.map_or_else(|| self.doc.length(&self.object), |heads| self.doc.length_at(&self.object, heads));
        if length > MAX_PHYSICAL_BYTES {
            return Err(Error::Limit);
        }
        let list = heads.map_or_else(|| self.doc.list_range(&self.object, ..), |heads| self.doc.list_range_at(&self.object, .., heads));
        let mut position = 0;
        list.map(|item| {
            let ValueRef::Scalar(ScalarValueRef::Str(text)) = &item.value else { return Err(invalid_effect()) };
            let mut scalars = text.chars();
            let scalar = scalars.next().filter(|_| scalars.next().is_none()).ok_or_else(invalid_effect)?;
            let atom = Atom { id: AtomId::new(item.id())?, scalar, position };
            position += scalar.len_utf8();
            Ok(atom)
        })
        .collect()
    }

    fn visible(&self, pending_deletes: &[AtomRun]) -> Result<Arc<Vec<Atom>>, Error> {
        if pending_deletes.is_empty()
            && let Some(visible) = self.visible.get()
        {
            return Ok(visible.clone());
        }
        let mut hidden = BTreeSet::new();
        for record in self.records.values() {
            let runs = if record.active { &record.effects.deletes } else { &record.births };
            hidden.extend(runs.iter().flat_map(AtomRun::ids));
        }
        hidden.extend(pending_deletes.iter().flat_map(AtomRun::ids));
        let visible = Arc::new(self.physical(None)?.into_iter().filter(|atom| !hidden.contains(&atom.id)).collect());
        if pending_deletes.is_empty() {
            let _ = self.visible.set(Arc::clone(&visible));
        }
        Ok(visible)
    }

    fn view(&self, styles: &Styles, pending_deletes: &[AtomRun]) -> Result<View, Error> {
        let visible = self.visible(pending_deletes)?;
        let mut display = DisplayFormats::default();
        display.apply(&self.initial);
        let mut records = self.records.iter().filter(|(_, record)| record.active).collect::<Vec<_>>();
        records.sort_unstable_by(|(a, x), (b, y)| (x.rank, a).cmp(&(y.rank, b)));
        for (_, record) in records {
            display.apply(&record.effects);
        }
        let mut projection = Projection { text: String::new(), chars: Vec::new(), paras: vec![display.first.paragraph_format(styles)?] };
        let mut last: Option<(&Properties, CharFormat)> = None;
        let plain = Properties::default();
        for atom in visible.iter() {
            projection.text.push(atom.scalar);
            let current = display.chars.get(&atom.id).unwrap_or(&plain);
            if last.as_ref().is_none_or(|(previous, _)| *previous != current) {
                last = Some((current, current.character_format(styles)?));
            }
            let format = &last.as_ref().ok_or_else(invalid_effect)?.1;
            if let Some(last) = projection.chars.last_mut().filter(|run| &run.format == format) {
                last.len += atom.scalar.len_utf8();
            } else {
                projection.chars.push(CharRun { len: atom.scalar.len_utf8(), format: format.clone() });
            }
            if atom.scalar == '\n' {
                let next = display.paragraphs.get(&atom.id).unwrap_or(&plain);
                projection.paras.push(next.paragraph_format(styles)?);
            }
            if projection.text.len() > super::MAX_STORY_BYTES
                || projection.paras.len() > super::MAX_STORY_PARAGRAPHS
                || projection.chars.len() > super::MAX_STORY_RUNS
            {
                return Err(Error::Limit);
            }
        }
        if visible.is_empty() {
            projection.chars.push(CharRun { len: 0, format: display.empty.character_format(styles)? });
        }
        Ok(View { projection, visible })
    }
}

#[derive(Default)]
struct DisplayFormats {
    chars: BTreeMap<AtomId, Properties>,
    paragraphs: BTreeMap<AtomId, Properties>,
    first: Properties,
    empty: Properties,
}

impl DisplayFormats {
    fn apply(&mut self, effects: &Effects) {
        for (target, formats) in [(&mut self.chars, &effects.characters), (&mut self.paragraphs, &effects.paragraphs)] {
            for effect in formats {
                for atom in effect.atoms.iter().flat_map(AtomRun::ids) {
                    target.entry(atom).or_default().update(&effect.writes);
                }
            }
        }
        self.first.update(&effects.first);
        self.empty.update(&effects.empty);
    }
}

impl Effects {
    fn is_empty(&self) -> bool {
        self.deletes.is_empty() && self.characters.is_empty() && self.paragraphs.is_empty() && self.first.is_empty() && self.empty.is_empty()
    }

    fn target_count(&self) -> Result<usize, Error> {
        self.deletes.iter().chain(self.characters.iter().chain(&self.paragraphs).flat_map(|effect| &effect.atoms)).try_fold(0usize, |count, run| {
            if !run.valid() {
                return Err(invalid_effect());
            }
            count.checked_add(run.count as usize).ok_or(Error::Limit)
        })
    }

    fn validate_targets(&self, old: &BTreeMap<AtomId, char>, new: &BTreeMap<AtomId, char>) -> Result<(), Error> {
        let mut deleted = BTreeSet::new();
        if !self.deletes.iter().flat_map(AtomRun::ids).all(|id| old.contains_key(&id) && deleted.insert(id)) {
            return Err(invalid_effect());
        }
        for (effects, paragraph) in [(&self.characters, false), (&self.paragraphs, true)] {
            for effect in effects {
                let mut targets = BTreeSet::new();
                if effect.atoms.is_empty()
                    || effect.writes.is_empty()
                    || !effect
                        .atoms
                        .iter()
                        .flat_map(AtomRun::ids)
                        .all(|id| targets.insert(id) && old.get(&id).or_else(|| new.get(&id)).is_some_and(|scalar| !paragraph || *scalar == '\n'))
                {
                    return Err(invalid_effect());
                }
            }
        }
        Ok(())
    }
}

fn format_effects(view: &View, target: &Story, styles: &Styles) -> Result<Effects, Error> {
    let before = &view.projection;
    if before.text != target.text {
        return Err(Error::UnsupportedEdit);
    }
    let mut effects = Effects {
        first: property_difference(&Properties::paragraph(&before.paras[0], styles)?, &Properties::paragraph(&target.paras[0], styles)?),
        ..Effects::default()
    };
    if view.visible.is_empty() {
        effects.empty =
            property_difference(&Properties::characters(&before.chars[0].format, styles)?, &Properties::characters(&target.chars[0].format, styles)?);
        return Ok(effects);
    }
    let mut atoms = view.visible.iter();
    let mut position = 0;
    let mut from = RunWalker::new(&before.chars);
    let mut to = RunWalker::new(&target.chars);
    while position < before.text.len() {
        let (old, terminal) = from.at(position).ok_or_else(invalid_effect)?;
        let (new, end) = to.at(position).ok_or_else(invalid_effect)?;
        let end = end.min(terminal);
        let writes = if old == new {
            Writes::new()
        } else {
            property_difference(&Properties::characters(old, styles)?, &Properties::characters(new, styles)?)
        };
        let mut runs = Vec::new();
        while position < end {
            let atom = atoms.next().ok_or_else(invalid_effect)?;
            position += atom.scalar.len_utf8();
            if !writes.is_empty() {
                AtomRun::push(&mut runs, atom.id);
            }
        }
        if !writes.is_empty() {
            effects.characters.push(PropertyEffect { atoms: runs, writes });
        }
    }
    let newlines = view.visible.iter().filter(|atom| atom.scalar == '\n');
    for (atom, (old, new)) in newlines.zip(before.paras[1..].iter().zip(&target.paras[1..])) {
        if old != new {
            let writes = property_difference(&Properties::paragraph(old, styles)?, &Properties::paragraph(new, styles)?);
            effects.paragraphs.push(PropertyEffect { atoms: vec![AtomRun { first: atom.id, count: 1 }], writes });
        }
    }
    Ok(effects)
}

struct RunWalker<'a> {
    remaining: std::slice::Iter<'a, CharRun>,
    current: Option<&'a CharRun>,
    end: usize,
}

impl<'a> RunWalker<'a> {
    fn new(runs: &'a [CharRun]) -> Self {
        Self { remaining: runs.iter(), current: None, end: 0 }
    }

    fn at(&mut self, position: usize) -> Option<(&'a CharFormat, usize)> {
        while self.end <= position {
            let next = self.remaining.next()?;
            self.current = Some(next);
            self.end += next.len;
        }
        Some((&self.current?.format, self.end))
    }
}

fn property_difference(before: &Properties, after: &Properties) -> Writes {
    after.changed_from(before).map(|(name, value)| (name.to_owned(), value.map(str::to_owned))).collect()
}

fn logical_index(visible: &[Atom], byte: usize) -> Option<usize> {
    let mut position = 0;
    for (index, atom) in visible.iter().enumerate() {
        if position >= byte {
            return (position == byte).then_some(index);
        }
        position += atom.scalar.len_utf8();
    }
    (position == byte).then_some(visible.len())
}

fn comment_ids(anchor: &ReversibleCommentAnchor) -> Result<Vec<AtomId>, Error> {
    let count = anchor.runs.iter().try_fold(0usize, |count, run| {
        if !run.valid() {
            return Err(invalid_effect());
        }
        count.checked_add(run.count as usize).ok_or(Error::Limit)
    })?;
    if count == 0 || count > MAX_EDIT_ATOMS {
        return Err(Error::Limit);
    }
    Ok(anchor.runs.iter().flat_map(AtomRun::ids).collect())
}

fn batch_key(actor: &str, sequence: u64) -> String {
    format!("{BATCH_KEY}:{actor}:{sequence}")
}

fn encode_effects(effects: &Effects) -> Result<Vec<u8>, Error> {
    let data = serde_json::to_vec(effects).map_err(|_| invalid_effect())?;
    if data.len() > MAX_EFFECT_BYTES { Err(Error::Limit) } else { Ok(data) }
}

fn only_revision_changed(before: &Story, after: &Story) -> bool {
    let mut before = before.clone();
    before.rev = after.rev;
    before == *after
}

fn invalid_effect() -> Error {
    Error::Invalid("reversible story edit")
}
