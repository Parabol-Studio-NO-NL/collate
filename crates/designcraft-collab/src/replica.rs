use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;

use designcraft_doc::{
    Content, Document, HyperlinkSource, Item, ItemId, ItemLoc, ParaFormat, PlaceholderAnchor, SpreadId, SpreadRef, StoryId, TextSel,
};

use crate::artwork::Placement;
use crate::text::{AtomId, CaptureHint, MAX_STORY_ATOMS, TextEdit, TextReplica, TextSplice, simple_text};
use crate::{
    CommentAnchor, CommentReply, CommentThread, Error, InitialTextFormat, MAX_OPERATIONS, Operation, ShapeFrame, SharedId, layers, pages, validation,
};

const MAX_ORDER_ITEMS: usize = 512;
const MAX_THREADS: usize = 512;
const MAX_REPLIES_PER_THREAD: usize = 64;
const MAX_COMMENT_ENTRIES: usize = 2_048;

/// Per-document identity and comments for a replica initialized from an identical seed.
#[derive(Clone)]
pub struct Replica {
    items: BTreeMap<SharedId, Option<ItemId>>,
    ids: BTreeMap<ItemId, SharedId>,
    comments: BTreeMap<SharedId, Arc<CommentThread>>,
    comment_ids: BTreeSet<SharedId>,
    text: BTreeMap<StoryId, TextReplica>,
}

impl Replica {
    pub fn new(document: &Document) -> Self {
        let ids: BTreeMap<_, _> =
            document.spreads.iter().flat_map(|spread| spread.items.iter()).map(|item| (item.id, SharedId::Seed(item.id))).collect();
        let items = ids.iter().map(|(local, shared)| (shared.clone(), Some(*local))).collect();
        Self { items, ids, comments: BTreeMap::new(), comment_ids: BTreeSet::new(), text: BTreeMap::new() }
    }

    pub fn shared_id(&self, local: ItemId) -> Option<SharedId> {
        self.ids.get(&local).cloned()
    }

    pub fn local_id(&self, shared: &SharedId) -> Option<ItemId> {
        self.items.get(shared).copied().flatten()
    }

    pub fn comments(&self) -> impl Iterator<Item = &CommentThread> {
        self.comments.values().map(Arc::as_ref)
    }

    pub fn comment_is_orphaned(&self, document: &Document, thread: &CommentThread) -> bool {
        match &thread.anchor {
            CommentAnchor::Page(id) => !document.spreads.iter().flat_map(|spread| &spread.pages).any(|page| page.id == *id),
            CommentAnchor::Object(id) => self.local_id(id).is_none_or(|local| document.item(local).is_none()),
            CommentAnchor::Text { .. } => self.comment_text_range(document, &thread.anchor).is_none(),
        }
    }

    /// Capture the original visible atoms of a nonempty UTF-8 byte range.
    pub fn text_comment_anchor(&self, document: &Document, story: StoryId, range: Range<usize>) -> Option<CommentAnchor> {
        let current = document.story(story)?;
        if current.text.get(range.clone())?.is_empty() {
            return None;
        }
        let atoms = match self.text.get(&story) {
            Some(replica) => replica.atoms_in(range),
            None => TextReplica::new(current, &document.styles).ok()?.atoms_in(range),
        }
        .ok()?;
        Some(CommentAnchor::Text { story, atoms })
    }

    /// Resolve a text comment to the span between its first and last surviving original atom.
    pub fn comment_text_range(&self, document: &Document, anchor: &CommentAnchor) -> Option<(StoryId, Range<usize>)> {
        let CommentAnchor::Text { story, atoms } = anchor else { return None };
        document.story(*story)?;
        let range = self.text.get(story)?.visible_anchored_range(atoms)?;
        Some((*story, range))
    }

    /// Capture a committed local edit while retaining the pre-edit replica. The
    /// engine can restore its local draft and apply the ordered host response.
    pub fn capture(&self, before: &Document, after: &Document, actor: &str, sequence: u64) -> Result<Vec<Operation>, Error> {
        self.capture_selected(before, after, actor, sequence, None)
    }

    pub fn capture_selected(
        &self,
        before: &Document,
        after: &Document,
        actor: &str,
        sequence: u64,
        text_selection: Option<(TextSel, TextSel)>,
    ) -> Result<Vec<Operation>, Error> {
        validation::actor(actor)?;
        if sequence == 0 {
            return Err(Error::Invalid("proposal sequence"));
        }
        if before == after {
            return Ok(Vec::new());
        }
        if let Some((edit, removed)) = pages::capture(before, after)? {
            self.frame_removals(before, &removed)?;
            return Ok(vec![Operation::Page { edit }]);
        }
        if let Some(edit) = layers::capture(before, after)? {
            let mut projected = before.clone();
            self.clone().apply_layer(&mut projected, &edit)?;
            if projected != *after {
                return Err(Error::UnsupportedEdit);
            }
            return Ok(vec![Operation::Layer { edit }]);
        }
        let old_items = spread_items(before);
        let new_items = spread_items(after);
        let mut ops = Vec::new();
        let mut created = BTreeMap::new();

        for (&local, &(spread, item)) in &old_items {
            if !new_items.contains_key(&local) {
                validation::editable_frame(item)?;
                let id = self.shared_id(local).ok_or(Error::UnknownObject)?;
                let delete = match item.text_frame() {
                    Some(frame) => Operation::DeleteText { id, spread, story: frame.story },
                    None => Operation::Delete { id, spread },
                };
                ops.push(delete);
            }
        }
        for spread in &after.spreads {
            for item in &spread.items {
                match old_items.get(&item.id) {
                    Some(&(old_spread, old)) if old_spread == spread.id => self.capture_existing(before, after, old, item, &mut ops)?,
                    Some(_) => return Err(Error::UnsupportedEdit),
                    None => {
                        validation::editable_frame(item)?;
                        let index = u16::try_from(ops.len()).map_err(|_| Error::Limit)?;
                        let id = SharedId::Created { actor: actor.to_owned(), sequence, index };
                        ops.push(capture_creation(before, after, item, id.clone(), spread.id)?);
                        created.insert(item.id, id);
                    }
                }
            }
        }
        self.capture_order(before, after, &created, &mut ops)?;
        self.capture_text(before, after, actor, sequence, &mut ops, text_selection)?;
        self.check_projection(before, after, &created, &ops)?;
        Ok(ops)
    }

    fn capture_existing(&self, before: &Document, after: &Document, old: &Item, new: &Item, ops: &mut Vec<Operation>) -> Result<(), Error> {
        if old != new {
            validation::editable_frame(old)?;
            validation::editable_frame(new)?;
            let id = self.shared_id(old.id).ok_or(Error::UnknownObject)?;
            let frame = ShapeFrame::from_item(old);
            ops.extend(ShapeFrame::from_item(new).changes(&frame).map(|change| Operation::Patch { id: id.clone(), change }));
            if old.graphic() != new.graphic() {
                let placement = new.graphic().map(|graphic| Placement::capture(graphic, before, after)).transpose()?.map(Box::new);
                ops.push(Operation::SetGraphic { id: id.clone(), placement });
            }
            if old.text_frame() != new.text_frame() {
                match new.text_frame() {
                    Some(frame) if matches!(old.content, Content::Unassigned) && before.story(frame.story).is_none() => {
                        ops.push(Operation::ConvertText { id });
                    }
                    _ => return Err(Error::UnsupportedEdit),
                }
            }
        }
        Ok(())
    }

    fn capture_order(
        &self,
        before: &Document,
        after: &Document,
        created: &BTreeMap<ItemId, SharedId>,
        ops: &mut Vec<Operation>,
    ) -> Result<(), Error> {
        if before.spreads.len() != after.spreads.len() {
            return Err(Error::UnsupportedEdit);
        }
        for (old, new) in before.spreads.iter().zip(&after.spreads) {
            if old.id != new.id {
                return Err(Error::UnsupportedEdit);
            }
            let after_order: Vec<_> = new.items.iter().map(|item| item.id).collect();
            let current_ids: BTreeSet<_> = after_order.iter().copied().collect();
            let natural_order = old
                .items
                .iter()
                .map(|item| item.id)
                .filter(|id| current_ids.contains(id))
                .chain(after_order.iter().copied().filter(|id| created.contains_key(id)));
            if natural_order.ne(after_order.iter().copied()) {
                let items = after_order
                    .into_iter()
                    .map(|id| created.get(&id).cloned().or_else(|| self.shared_id(id)).ok_or(Error::UnknownObject))
                    .collect::<Result<Vec<_>, _>>()?;
                ops.push(Operation::Reorder { spread: new.id, items });
            }
        }
        Ok(())
    }

    fn capture_text(
        &self,
        before: &Document,
        after: &Document,
        actor: &str,
        sequence: u64,
        operations: &mut Vec<Operation>,
        selection: Option<(TextSel, TextSel)>,
    ) -> Result<(), Error> {
        for (&id, current) in &after.stories {
            // New stories belong to their frame operation. The whole-model projection rejects any
            // story that the frame operation cannot reproduce.
            let Some(previous) = before.stories.get(&id) else { continue };
            if Arc::ptr_eq(previous, current) || previous == current {
                continue;
            }
            let replica = self.text.get(&id).cloned().map_or_else(|| TextReplica::new(previous, &before.styles), Ok)?;
            let hint = selection
                .filter(|(old, new)| old.story == id && new.story == id && old.cell.is_none() && new.cell.is_none())
                .and_then(|(old, new)| text_splice_hint(old, new, previous.text.len(), current.text.len()));
            let edits = replica.capture_with_hint(previous, current, &before.styles, actor, sequence, hint)?;
            if !edits.is_empty() {
                operations.push(Operation::Text { story: id, actor: actor.into(), sequence, edits });
            }
        }
        Ok(())
    }

    /// A sticky predecessor for a local selection. The outer option is absent for unsupported stories.
    pub fn text_anchor(&self, document: &Document, story: StoryId, byte: usize) -> Option<Option<AtomId>> {
        if let Some(replica) = self.text.get(&story) {
            return replica.atom_before(byte).ok();
        }
        TextReplica::new(document.story(story)?, &document.styles).ok()?.atom_before(byte).ok()
    }

    /// Recover a UTF-8 byte offset from a surviving or tombstoned anchor.
    pub fn text_byte_after(&self, story: StoryId, anchor: Option<&AtomId>) -> Option<usize> {
        match anchor {
            None => Some(0),
            Some(atom) => self.text.get(&story)?.byte_after(atom).ok(),
        }
    }

    // Replay the constrained mutations into a copy. Comparing the whole
    // result prevents a mixed edit from quietly dropping its unsupported part.
    fn check_projection(&self, before: &Document, after: &Document, created: &BTreeMap<ItemId, SharedId>, ops: &[Operation]) -> Result<(), Error> {
        let mut projected = before.clone();
        let mut replica = self.clone();
        replica.apply_batch(&mut projected, ops)?;
        let local_ids: BTreeMap<_, _> = created
            .iter()
            .map(|(&original, shared)| Ok((replica.local_id(shared).ok_or(Error::UnknownObject)?, original)))
            .collect::<Result<_, Error>>()?;
        // Canonical allocation can spend IDs differently from the clipboard, for example when
        // a graphic reuses an existing asset. Map each new frame and its own story back to the draft.
        let mut new_stories = Vec::new();
        for op in ops {
            let Operation::CreateText { id, .. } = op else { continue };
            let canonical = replica.local_id(id).ok_or(Error::UnknownObject)?;
            let draft = *local_ids.get(&canonical).ok_or(Error::UnknownObject)?;
            let canonical_story = projected.item(canonical).and_then(Item::text_frame).ok_or(Error::UnknownResource)?.story;
            let draft_story = after.item(draft).and_then(Item::text_frame).ok_or(Error::UnknownResource)?.story;
            let mut story = projected.stories.remove(&canonical_story).ok_or(Error::UnknownResource)?;
            let model = Arc::make_mut(&mut story);
            model.id = draft_story;
            model.rev = after.story(draft_story).ok_or(Error::UnknownResource)?.rev;
            model.frames = vec![draft];
            projected.item_mut(canonical).and_then(|item| item.text_frame_mut()).ok_or(Error::UnknownResource)?.story = draft_story;
            new_stories.push((draft_story, story));
        }
        for (id, story) in new_stories {
            if projected.stories.insert(id, story).is_some() {
                return Err(Error::UnsupportedEdit);
            }
        }
        for spread in &mut projected.spreads {
            if spread.items.iter().any(|item| local_ids.contains_key(&item.id)) {
                for item in &mut Arc::make_mut(spread).items {
                    if let Some(&original) = local_ids.get(&item.id) {
                        Arc::make_mut(item).id = original;
                    }
                }
            }
        }
        match_created_assets(before, after, &mut projected, local_ids.values().copied())?;
        for operation in ops {
            if let Operation::Text { story, .. } = operation
                && let (Some(projected), Some(observed)) = (projected.story_mut(*story), after.story(*story))
            {
                projected.rev = observed.rev;
            }
        }
        // Creations may have provisional IDs. Removing a text frame does not allocate anything.
        if !matches!(ops, [Operation::DeleteText { .. }]) {
            projected.next_id = after.next_id;
        }
        if projected == *after { Ok(()) } else { Err(Error::UnsupportedEdit) }
    }

    /// Compute compensation from the replica and document immediately before an accepted batch.
    /// Structural edits and batches whose previous values cannot be shared have no compensation.
    /// The host checks later conflicting edits before applying the result to its current state.
    pub fn inverse_patch_batch(&self, before: &Document, edits: &[Operation]) -> Result<Option<Vec<Operation>>, Error> {
        if edits.len() > MAX_OPERATIONS {
            return Err(Error::Limit);
        }
        if edits.is_empty()
            || edits.iter().any(|edit| {
                !matches!(
                    edit,
                    Operation::Patch { .. }
                        | Operation::Reorder { .. }
                        | Operation::SetResolved { .. }
                        | Operation::Text { .. }
                        | Operation::SetTextBatchActive { .. }
                )
            })
        {
            return Ok(None);
        }
        let mut inverse = Vec::with_capacity(edits.len());
        for edit in edits.iter().rev() {
            let restore = match edit {
                Operation::Patch { id, change } => {
                    let loc = self.item_location(before, id)?;
                    let item = before.item_at(&loc).ok_or(Error::UnknownObject)?;
                    let change = change.previous(item);
                    if validation::change(before, &change).is_err() {
                        return Ok(None);
                    }
                    Operation::Patch { id: id.clone(), change }
                }
                Operation::Reorder { spread, .. } => {
                    let old = before.spread(spread_ref(before, *spread)?).ok_or(Error::UnknownResource)?;
                    let items = old.items.iter().map(|item| self.shared_id(item.id).ok_or(Error::UnknownObject)).collect::<Result<Vec<_>, _>>()?;
                    Operation::Reorder { spread: *spread, items }
                }
                Operation::SetResolved { thread, .. } => {
                    let previous = self.comments.get(thread).ok_or(Error::UnknownResource)?;
                    Operation::SetResolved { thread: thread.clone(), resolved: previous.resolved }
                }
                Operation::Text { story, actor, sequence, edits } if !edits.is_empty() => {
                    Operation::SetTextBatchActive { story: *story, actor: actor.clone(), original_sequence: *sequence, active: false }
                }
                Operation::SetTextBatchActive { story, actor, original_sequence, active } => {
                    Operation::SetTextBatchActive { story: *story, actor: actor.clone(), original_sequence: *original_sequence, active: !active }
                }
                Operation::Create { .. }
                | Operation::CreateGraphic { .. }
                | Operation::CreateText { .. }
                | Operation::ConvertText { .. }
                | Operation::Delete { .. }
                | Operation::DeleteText { .. }
                | Operation::SetGraphic { .. }
                | Operation::Page { .. }
                | Operation::Layer { .. }
                | Operation::Text { .. }
                | Operation::CreateComment { .. }
                | Operation::Reply { .. } => return Ok(None),
            };
            inverse.push(restore);
        }
        Ok(Some(inverse))
    }

    /// Apply a host-ordered batch atomically. The room owns revision ordering
    /// and filters duplicate envelopes before calling this method.
    pub fn apply(&mut self, document: &mut Document, operations: &[Operation]) -> Result<(), Error> {
        if operations.is_empty() {
            return Ok(());
        }
        let mut staged_document = document.clone();
        let mut staged_replica = self.clone();
        staged_replica.apply_batch(&mut staged_document, operations)?;
        *document = staged_document;
        *self = staged_replica;
        Ok(())
    }

    fn apply_batch(&mut self, document: &mut Document, operations: &[Operation]) -> Result<(), Error> {
        if operations.len() > MAX_OPERATIONS {
            return Err(Error::Limit);
        }
        // Structural edits must pass conflict checks as their own transaction.
        if operations.len() > 1
            && operations.iter().any(|operation| matches!(operation, Operation::Page { .. } | Operation::Layer { .. } | Operation::DeleteText { .. }))
        {
            return Err(Error::UnsupportedEdit);
        }
        let mut updated_stories = BTreeSet::new();
        for op in operations {
            match op {
                Operation::Create { id, spread, frame } => self.create(document, id, *spread, frame, None)?,
                Operation::CreateGraphic { id, spread, frame, placement } => self.create(document, id, *spread, frame, Some(placement))?,
                Operation::CreateText { id, spread, frame, text, vertical, format } => {
                    let story = self.create_text(document, id, *spread, frame, text, *vertical)?;
                    if let Some(format) = format {
                        let styles = document.styles.clone();
                        let story = document.story_mut(story).ok_or(Error::UnknownResource)?;
                        story.chars.clone_from(&format.chars);
                        story.paras.clone_from(&format.paras);
                        TextReplica::new(story, &styles)?;
                    }
                }
                Operation::ConvertText { id } => self.convert_text(document, id)?,
                Operation::Delete { id, spread } => self.delete(document, id, *spread, None)?,
                Operation::DeleteText { id, spread, story } => self.delete(document, id, *spread, Some(*story))?,
                Operation::Patch { id, change } => {
                    let loc = self.item_location(document, id)?;
                    validation::change(document, change)?;
                    change.apply(document.item_mut_at(&loc).ok_or(Error::UnknownObject)?);
                }
                Operation::SetGraphic { id, placement } => {
                    let loc = self.item_location(document, id)?;
                    if document.item_at(&loc).is_some_and(|item| item.text_frame().is_some()) {
                        return Err(Error::UnsupportedObject);
                    }
                    let graphic = placement.as_ref().map(|value| value.install(document)).transpose()?;
                    document.item_mut_at(&loc).ok_or(Error::UnknownObject)?.content = graphic.map_or(Content::Unassigned, Content::Graphic);
                }
                Operation::Reorder { spread, items } => self.reorder(document, *spread, items)?,
                Operation::Page { edit } => self.apply_page(document, edit)?,
                Operation::Layer { edit } => self.apply_layer(document, edit)?,
                Operation::Text { story, actor, sequence, edits } => {
                    if !updated_stories.insert(*story) {
                        return Err(Error::DuplicateId);
                    }
                    self.apply_text(document, *story, actor, *sequence, edits)?;
                }
                Operation::SetTextBatchActive { story, actor, original_sequence, active } => {
                    if !updated_stories.insert(*story) {
                        return Err(Error::DuplicateId);
                    }
                    let replica = self.text.get_mut(story).ok_or(Error::UnknownResource)?;
                    let story = document.story_mut(*story).ok_or(Error::UnknownResource)?;
                    replica.set_batch_active(story, actor, *original_sequence, *active)?;
                }
                Operation::CreateComment { id, anchor, text } => self.create_comment(document, id, anchor, text)?,
                Operation::Reply { thread, id, text } => self.reply(thread, id, text)?,
                Operation::SetResolved { thread, resolved } => self.comment_mut(thread)?.resolved = *resolved,
            }
        }
        Ok(())
    }

    /// A story deleted by a later structural edit cannot supply text history.
    pub fn can_activate_text_batch(&self, story: StoryId, actor: &str, sequence: u64, active: bool) -> bool {
        self.text.get(&story).is_some_and(|replica| replica.can_set_batch_active(actor, sequence, active))
    }

    fn apply_page(&mut self, document: &mut Document, edit: &pages::PageEdit) -> Result<(), Error> {
        let mut projected = document.clone();
        let removed = pages::apply(&mut projected, edit)?;
        for (local, id, story) in self.frame_removals(document, &removed)? {
            self.tombstone_frame(local, id, story);
        }
        *document = projected;
        Ok(())
    }

    fn apply_layer(&mut self, document: &mut Document, edit: &layers::LayerEdit) -> Result<(), Error> {
        let before = document.clone();
        let removed = layers::apply(document, edit)?;
        for (local, id, story) in self.frame_removals(&before, &removed)? {
            self.tombstone_frame(local, id, story);
        }
        Ok(())
    }

    fn frame_removals(&self, document: &Document, removed: &[ItemId]) -> Result<Vec<(ItemId, SharedId, Option<StoryId>)>, Error> {
        if removed.is_empty() {
            return Ok(Vec::new());
        }
        let expected: BTreeSet<_> = removed.iter().copied().collect();
        let frames = document
            .spreads
            .iter()
            .flat_map(|spread| &spread.items)
            .filter(|item| expected.contains(&item.id))
            .map(|item| {
                let shared = self.shared_id(item.id).ok_or(Error::UnknownObject)?;
                Ok((item.id, shared, validate_frame_removal(document, item)?))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        if frames.len() != expected.len() {
            return Err(Error::UnknownObject);
        }
        Ok(frames)
    }

    fn create_text(
        &mut self,
        document: &mut Document,
        id: &SharedId,
        spread: SpreadId,
        frame: &ShapeFrame,
        text: &str,
        vertical: bool,
    ) -> Result<StoryId, Error> {
        self.check_new_id(id)?;
        validation::created_frame(document, frame)?;
        validate_initial_text(text)?;
        document.next_id.checked_add(2).ok_or(Error::Limit)?;
        let destination = spread_ref(document, spread)?;
        let rect = frame.geometry.path.bounds().ok_or(Error::Invalid("text frame"))?;
        let para = ParaFormat { style: document.styles.default_paragraph.clone(), ..Default::default() };
        let (local, story) = document.add_text_frame(destination, rect, frame.layer, text, para).map_err(|_| Error::UnknownResource)?;
        let item = document.item_mut(local).ok_or(Error::UnknownObject)?;
        frame.geometry.apply(item);
        item.fill.clone_from(&frame.fill);
        item.stroke.clone_from(&frame.stroke);
        frame.appearance.apply(item);
        if vertical {
            let model = document.story_mut(story).ok_or(Error::UnknownResource)?;
            model.vertical = true;
            model.rev = model.rev.wrapping_add(1);
        }
        self.ids.insert(local, id.clone());
        self.items.insert(id.clone(), Some(local));
        Ok(story)
    }

    fn convert_text(&self, document: &mut Document, id: &SharedId) -> Result<(), Error> {
        let loc = self.item_location(document, id)?;
        let item = document.item_at(&loc).ok_or(Error::UnknownObject)?;
        if !matches!(item.content, Content::Unassigned) || item.locked || document.layer(item.layer).is_some_and(|layer| layer.locked) {
            return Err(Error::UnsupportedObject);
        }
        let local = item.id;
        document.next_id.checked_add(1).ok_or(Error::Limit)?;
        document.convert_frame_to_text(local).map_err(|_| Error::UnknownObject)?;
        Ok(())
    }

    fn apply_text(&mut self, document: &mut Document, id: StoryId, actor: &str, sequence: u64, edits: &[TextEdit]) -> Result<(), Error> {
        let styles = document.styles.clone();
        let story = document.story_mut(id).ok_or(Error::UnknownResource)?;
        let replica = match self.text.entry(id) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => entry.insert(TextReplica::new(story, &styles)?),
        };
        replica.apply(story, &styles, actor, sequence, edits)
    }

    fn create(
        &mut self,
        document: &mut Document,
        id: &SharedId,
        spread: SpreadId,
        frame: &ShapeFrame,
        placement: Option<&Placement>,
    ) -> Result<(), Error> {
        self.check_new_id(id)?;
        validation::created_frame(document, frame)?;
        let destination = spread_ref(document, spread)?;
        let graphic = placement.map(|placement| placement.install(document)).transpose()?;
        let local = ItemId(document.next_id.checked_add(1).ok_or(Error::Limit)?);
        if document.item(local).is_some() || document.anchored_item(local).is_some() {
            return Err(Error::DuplicateId);
        }
        let mut item = frame.to_item(local);
        if let Some(graphic) = graphic {
            item.content = Content::Graphic(graphic);
            if let Some(effects) = document.styles.object_effects(&item.object_style) {
                item.effects = effects.clone();
            }
        }
        document.insert_item(destination, item, None).map_err(|_| Error::UnknownResource)?;
        document.next_id = local.0;
        self.ids.insert(local, id.clone());
        self.items.insert(id.clone(), Some(local));
        Ok(())
    }

    fn delete(&mut self, document: &mut Document, id: &SharedId, spread: SpreadId, story: Option<StoryId>) -> Result<(), Error> {
        let loc = self.item_location(document, id)?;
        if document.spread(loc.spread).is_none_or(|current| current.id != spread) {
            return Err(Error::UnknownResource);
        }
        let item = document.item_at(&loc).ok_or(Error::UnknownObject)?;
        let local = item.id;
        if item.text_frame().map(|frame| frame.story) != story {
            return Err(Error::UnsupportedObject);
        }
        validate_frame_removal(document, item)?;
        document.remove_item(local).map_err(|_| Error::UnknownObject)?;
        self.tombstone_frame(local, id.clone(), story);
        Ok(())
    }

    fn tombstone_frame(&mut self, local: ItemId, id: SharedId, story: Option<StoryId>) {
        self.ids.remove(&local);
        self.items.insert(id, None);
        if let Some(story) = story {
            self.text.remove(&story);
        }
    }

    fn reorder(&self, document: &mut Document, id: SpreadId, order: &[SharedId]) -> Result<(), Error> {
        if order.len() > MAX_ORDER_ITEMS {
            return Err(Error::Limit);
        }
        let loc = spread_ref(document, id)?;
        let spread = document.spread_mut(loc).ok_or(Error::UnknownResource)?;
        if spread.items.len() != order.len() {
            return Err(Error::Invalid("stack order"));
        }
        let mut by_id: BTreeMap<_, _> = spread.items.iter().map(|item| (item.id, Arc::clone(item))).collect();
        let mut items = Vec::with_capacity(order.len());
        for id in order {
            let local = self.local_id(id).ok_or(Error::UnknownObject)?;
            items.push(by_id.remove(&local).ok_or(Error::Invalid("stack order"))?);
        }
        spread.items = items;
        Ok(())
    }

    fn item_location(&self, document: &Document, id: &SharedId) -> Result<ItemLoc, Error> {
        let local = self.local_id(id).ok_or(Error::UnknownObject)?;
        let loc = document.find(local).ok_or(Error::UnknownObject)?;
        if !matches!(loc.spread, SpreadRef::Doc(_)) || loc.path.len() != 1 {
            return Err(Error::UnsupportedObject);
        }
        validation::editable_frame(document.item_at(&loc).ok_or(Error::UnknownObject)?)?;
        Ok(loc)
    }

    fn check_new_id(&self, id: &SharedId) -> Result<(), Error> {
        validation::new_id(id)?;
        if self.items.contains_key(id) || self.comment_ids.contains(id) {
            return Err(Error::DuplicateId);
        }
        Ok(())
    }

    fn create_comment(&mut self, document: &Document, id: &SharedId, anchor: &CommentAnchor, text: &str) -> Result<(), Error> {
        self.check_new_id(id)?;
        validation::comment(text)?;
        if self.comments.len() >= MAX_THREADS || self.comment_ids.len() >= MAX_COMMENT_ENTRIES {
            return Err(Error::Limit);
        }
        match anchor {
            CommentAnchor::Page(id) if !document.spreads.iter().flat_map(|spread| &spread.pages).any(|page| page.id == *id) => {
                return Err(Error::UnknownResource);
            }
            CommentAnchor::Object(id) if !self.items.contains_key(id) => return Err(Error::UnknownObject),
            CommentAnchor::Text { story, atoms } => {
                let current = document.story(*story).ok_or(Error::UnknownResource)?;
                let replica = match self.text.entry(*story) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => entry.insert(TextReplica::new(current, &document.styles)?),
                };
                replica.validate_comment_atoms(atoms)?;
            }
            _ => {}
        }
        let thread = CommentThread { id: id.clone(), anchor: anchor.clone(), text: text.to_owned(), replies: Vec::new(), resolved: false };
        self.comments.insert(id.clone(), Arc::new(thread));
        self.comment_ids.insert(id.clone());
        Ok(())
    }

    fn reply(&mut self, thread: &SharedId, id: &SharedId, text: &str) -> Result<(), Error> {
        self.check_new_id(id)?;
        validation::comment(text)?;
        if self.comment_ids.len() >= MAX_COMMENT_ENTRIES {
            return Err(Error::Limit);
        }
        let thread = self.comment_mut(thread)?;
        if thread.replies.len() >= MAX_REPLIES_PER_THREAD {
            return Err(Error::Limit);
        }
        thread.replies.push(CommentReply { id: id.clone(), text: text.to_owned() });
        self.comment_ids.insert(id.clone());
        Ok(())
    }

    fn comment_mut(&mut self, id: &SharedId) -> Result<&mut CommentThread, Error> {
        self.comments.get_mut(id).map(Arc::make_mut).ok_or(Error::UnknownResource)
    }
}

/// Copying artwork from another document can allocate its frame before the asset, while the
/// room installs the asset first. Align only provisional assets for the whole-model comparison.
fn match_created_assets(before: &Document, after: &Document, projected: &mut Document, items: impl Iterator<Item = ItemId>) -> Result<(), Error> {
    let mut mapping = HashMap::new();
    for item in items {
        let from = projected.item(item).and_then(Item::graphic).map(|graphic| graphic.asset);
        let to = after.item(item).and_then(Item::graphic).map(|graphic| graphic.asset);
        let (Some(from), Some(to)) = (from, to) else { continue };
        if from != to
            && (before.assets.contains_key(&from) || before.assets.contains_key(&to) || mapping.insert(from, to).is_some_and(|prior| prior != to))
        {
            return Err(Error::UnsupportedEdit);
        }
    }
    let mut assets = Vec::new();
    for (&from, &to) in &mapping {
        let mut asset = projected.assets.remove(&from).ok_or(Error::UnknownResource)?;
        Arc::make_mut(&mut asset).id = to;
        assets.push((to, asset));
    }
    for (id, asset) in assets {
        if projected.assets.insert(id, asset).is_some() {
            return Err(Error::UnsupportedEdit);
        }
    }
    projected.remap_assets(&mapping);
    Ok(())
}

fn validate_initial_text(text: &str) -> Result<(), Error> {
    if text.len() > MAX_STORY_ATOMS * 4 || text.chars().count() > MAX_STORY_ATOMS {
        return Err(Error::Limit);
    }
    if !simple_text(text) {
        return Err(Error::Invalid("initial story text"));
    }
    Ok(())
}

fn capture_creation(before: &Document, after: &Document, item: &Item, id: SharedId, spread: SpreadId) -> Result<Operation, Error> {
    let frame = Box::new(ShapeFrame::from_item(item));
    match &item.content {
        Content::Unassigned => Ok(Operation::Create { id, spread, frame }),
        Content::Graphic(graphic) => {
            let placement = Box::new(Placement::capture(graphic, before, after)?);
            Ok(Operation::CreateGraphic { id, spread, frame, placement })
        }
        Content::Text(text) if before.story(text.story).is_none() => {
            let story = after.story(text.story).ok_or(Error::UnknownResource)?;
            if story.frames != [item.id] {
                return Err(Error::UnsupportedObject);
            }
            let default_para = ParaFormat { style: after.styles.default_paragraph.clone(), ..Default::default() };
            let format =
                (story.chars.len() != 1 || story.chars[0].format != Default::default() || story.paras.iter().any(|para| *para != default_para))
                    .then(|| Box::new(InitialTextFormat { chars: story.chars.clone(), paras: story.paras.clone() }));
            Ok(Operation::CreateText { id, spread, frame, text: story.text.clone(), vertical: story.vertical, format })
        }
        _ => Err(Error::UnsupportedObject),
    }
}

fn spread_items(document: &Document) -> BTreeMap<ItemId, (SpreadId, &Item)> {
    document.spreads.iter().flat_map(|spread| spread.items.iter().map(|item| (item.id, (spread.id, item.as_ref())))).collect()
}

fn spread_ref(document: &Document, id: SpreadId) -> Result<SpreadRef, Error> {
    document.spreads.iter().position(|spread| spread.id == id).map(SpreadRef::Doc).ok_or(Error::UnknownResource)
}

fn validate_frame_removal(document: &Document, item: &Item) -> Result<Option<StoryId>, Error> {
    validation::editable_frame(item)?;
    if item_is_referenced(document, item.id) {
        return Err(Error::UnsupportedObject);
    }
    let story = item.text_frame().map(|frame| frame.story);
    if let Some(story) = story {
        let current = document.story(story).ok_or(Error::UnknownResource)?;
        if current.frames != [item.id] || story_is_referenced(document, story) {
            return Err(Error::UnsupportedObject);
        }
        TextReplica::new(current, &document.styles)?;
    }
    Ok(story)
}

fn item_is_referenced(document: &Document, id: ItemId) -> bool {
    document.articles.iter().any(|article| article.items.contains(&id))
        || document.hyperlinks.iter().any(|link| matches!(link.source, HyperlinkSource::Item { id: target } if target == id))
        || document
            .data_merge
            .placeholders
            .iter()
            .any(|placeholder| matches!(placeholder.anchor, PlaceholderAnchor::Item { id: target } if target == id))
}

fn story_is_referenced(document: &Document, id: StoryId) -> bool {
    document.settings.primary_story == Some(id)
        || document.endnote_story == Some(id)
        || document.toc.as_ref().is_some_and(|toc| toc.story == id)
        || document.index.as_ref().is_some_and(|index| index.story == id)
        || document.stories.values().any(|story| story.link.is_some_and(|(source, _)| source == id))
        || document.hyperlinks.iter().any(|link| matches!(link.source, HyperlinkSource::Text { story, .. } if story == id))
        || document
            .data_merge
            .placeholders
            .iter()
            .any(|placeholder| matches!(placeholder.anchor, PlaceholderAnchor::Text { story, .. } if story == id))
}

fn text_splice_hint(before: TextSel, after: TextSel, old_len: usize, new_len: usize) -> Option<CaptureHint> {
    if !after.is_caret() {
        if (before.anchor < before.focus) != (after.anchor < after.focus) {
            return None;
        }
        return CaptureHint::delete_surrounding(before.range(), after.range(), old_len, new_len);
    }
    let end = after.focus;
    if !before.is_caret() {
        let start = before.anchor.min(before.focus);
        let old_end = before.anchor.max(before.focus);
        return (start <= end).then(|| TextSplice { old: start..old_end, new: start..end }.into());
    }
    if new_len >= old_len {
        return (before.focus <= end).then(|| TextSplice { old: before.focus..before.focus, new: before.focus..end }.into());
    }
    let old_end = end.checked_add(old_len - new_len)?;
    Some(TextSplice { old: end..old_end, new: end..end }.into())
}
