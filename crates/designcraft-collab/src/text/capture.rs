use std::{ops::Range, sync::Arc};

use super::{
    AtomId, BasicTextFormat, Character, Error, MAX_BATCH_ATOMS, MAX_EDIT_ATOMS, MAX_OPERATIONS, Story, Styles, TextEdit, TextFormatChange,
    TextReplica, validation,
};

/// UTF-8 byte ranges for the editor's actual replacement, interpreted in their respective snapshots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextSplice {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

impl TextSplice {
    fn matching_edges(&self, before: &str, after: &str) -> Option<(usize, usize)> {
        if self.old.start > self.old.end || self.new.start > self.new.end {
            return None;
        }
        // Replacing a newline with the same bytes still removes its old paragraph identity.
        if before == after && !before.get(self.old.clone())?.contains('\n') {
            return None;
        }
        let prefix = before.get(..self.old.start)?;
        let suffix = before.get(self.old.end..)?;
        if Some(prefix) != after.get(..self.new.start) || Some(suffix) != after.get(self.new.end..) {
            return None;
        }
        Some((prefix.chars().count(), suffix.chars().count()))
    }
}

pub(crate) struct CaptureHint {
    splice: TextSplice,
    retained: Option<Range<usize>>,
}

impl From<TextSplice> for CaptureHint {
    fn from(splice: TextSplice) -> Self {
        Self { splice, retained: None }
    }
}

impl CaptureHint {
    pub(crate) fn delete_surrounding(old: Range<usize>, new: Range<usize>, old_len: usize, new_len: usize) -> Option<Self> {
        let removed = old_len.checked_sub(new_len)?;
        if removed == 0 || old.is_empty() || old.len() != new.len() {
            return None;
        }
        let before = old.start.checked_sub(new.start)?;
        let after = removed.checked_sub(before)?;
        let splice = TextSplice { old: new.start..old.end.checked_add(after)?, new };
        Some(Self { splice, retained: Some(old) })
    }

    fn matching_text(self, before: &str, after: &str) -> Option<(usize, usize, Option<Range<usize>>)> {
        let (prefix, suffix) = self.splice.matching_edges(before, after)?;
        let retained = if let Some(range) = self.retained {
            let text = before.get(range.clone())?;
            if text != after.get(self.splice.new)? {
                return None;
            }
            let start = prefix + before.get(self.splice.old.start..range.start)?.chars().count();
            Some(start..start + text.chars().count())
        } else {
            None
        };
        Some((prefix, suffix, retained))
    }
}

impl TextReplica {
    /// Translate an ordinary single-range editor change into stable text operations. Independent character formatting
    /// changes on surviving text are captured per attribute. The generated batch must reproduce the complete draft.
    pub fn capture(&self, before: &Story, after: &Story, styles: &Styles, actor: &str, sequence: u64) -> Result<Vec<TextEdit>, Error> {
        self.capture_at(before, after, styles, actor, sequence, None)
    }

    /// Prefer a valid editor splice so edits beside identical characters retain the intended atom identities.
    /// If the hint cannot account for the snapshots, infer a minimal scalar splice.
    pub fn capture_at(
        &self,
        before: &Story,
        after: &Story,
        styles: &Styles,
        actor: &str,
        sequence: u64,
        hint: Option<TextSplice>,
    ) -> Result<Vec<TextEdit>, Error> {
        self.capture_with_hint(before, after, styles, actor, sequence, hint.map(Into::into))
    }

    pub(crate) fn capture_with_hint(
        &self,
        before: &Story,
        after: &Story,
        styles: &Styles,
        actor: &str,
        sequence: u64,
        hint: Option<CaptureHint>,
    ) -> Result<Vec<TextEdit>, Error> {
        validation::actor(actor)?;
        if sequence == 0 {
            return Err(Error::Invalid("proposal sequence"));
        }
        self.check_projection(before, &self.projection())?;
        let after_replica = Self::new(after, styles)?;
        let old: Vec<_> = self.visible().collect();
        let new: Vec<_> = after_replica.visible().map(|(_, character)| character).collect();
        let (prefix, suffix, retained) = hint.and_then(|hint| hint.matching_text(&before.text, &after.text)).unwrap_or_else(|| {
            let prefix = old.iter().zip(&new).take_while(|((_, a), b)| a.scalar == b.scalar).count();
            let suffix = old[prefix..].iter().rev().zip(new[prefix..].iter().rev()).take_while(|((_, a), b)| a.scalar == b.scalar).count();
            (prefix, suffix, None)
        });
        let old_end = old.len() - suffix;
        let added_end = if retained.is_some() { prefix } else { new.len() - suffix };
        let retained = retained.unwrap_or(old_end..old_end);
        let added = &new[prefix..added_end];
        if old_end - prefix - retained.len() + added.len() > MAX_BATCH_ATOMS {
            return Err(Error::Limit);
        }

        let mut edits: Vec<_> = old[prefix..retained.start]
            .chunks(MAX_EDIT_ATOMS)
            .chain(old[retained.end..old_end].chunks(MAX_EDIT_ATOMS))
            .map(|ids| TextEdit::Delete { atoms: ids.iter().map(|(id, _)| (*id).clone()).collect() })
            .collect();
        let anchor = prefix.checked_sub(1).map(|index| old[index].0.clone());
        append_insertions(&mut edits, added, anchor, actor.into(), sequence)?;
        let unchanged = old[..prefix].iter().chain(&old[retained]).chain(&old[old_end..]).zip(new[..prefix].iter().chain(&new[added_end..]));
        for ((id, previous), current) in unchanged {
            for change in current.format.changes(&previous.format) {
                append_formatting(&mut edits, id, change)?;
            }
        }

        let mut projected = before.clone();
        let mut staged = self.clone();
        staged.apply(&mut projected, styles, actor, sequence, &edits)?;

        // Map the draft's paragraph formats to the stable anchors in the projected text. A
        // separator inserted by this batch inherits its paragraph's format before local overrides.
        let text_edit_count = edits.len();
        for ((after, current), (_, draft)) in staged.visible_paragraphs().zip(after_replica.visible_paragraphs()) {
            for change in draft.current.changes(&current.current) {
                if edits.len() == MAX_OPERATIONS {
                    return Err(Error::Limit);
                }
                edits.push(TextEdit::FormatParagraph { after: after.cloned(), change });
            }
        }
        if edits.len() != text_edit_count {
            projected = before.clone();
            self.clone().apply(&mut projected, styles, actor, sequence, &edits)?;
        }
        projected.rev = after.rev;
        if projected != *after {
            return Err(Error::UnsupportedEdit);
        }
        Ok(edits)
    }
}

fn append_insertions(
    edits: &mut Vec<TextEdit>,
    added: &[&Character],
    mut anchor: Option<AtomId>,
    actor: Arc<str>,
    sequence: u64,
) -> Result<(), Error> {
    let mut index = 0;
    for run in added.chunk_by(|a, b| a.format == b.format) {
        for fragment in run.chunks(MAX_EDIT_ATOMS) {
            if edits.len() == MAX_OPERATIONS {
                return Err(Error::Limit);
            }
            let text = fragment.iter().map(|character| character.scalar).collect();
            edits.push(TextEdit::Insert { after: anchor, text, format: fragment[0].format.as_ref().clone() });
            index += fragment.len();
            anchor = Some(AtomId::Inserted { actor: actor.clone(), sequence, index: (index - 1) as u16 });
        }
    }
    Ok(())
}

fn append_formatting(edits: &mut Vec<TextEdit>, id: &AtomId, change: TextFormatChange) -> Result<(), Error> {
    if let Some(targets) = edits.iter_mut().find_map(|edit| match edit {
        TextEdit::Format { atoms, change: previous } if *previous == change && atoms.len() < MAX_EDIT_ATOMS => Some(atoms),
        _ => None,
    }) {
        targets.push(id.clone());
    } else {
        if edits.len() == MAX_OPERATIONS {
            return Err(Error::Limit);
        }
        edits.push(TextEdit::Format { atoms: vec![id.clone()], change });
    }
    Ok(())
}

impl BasicTextFormat {
    fn changes(&self, before: &Self) -> impl Iterator<Item = TextFormatChange> {
        [
            (self.style != before.style).then(|| TextFormatChange::Style(self.style.clone())),
            (self.font_family != before.font_family).then(|| TextFormatChange::FontFamily(self.font_family.clone())),
            (self.font_style != before.font_style).then(|| TextFormatChange::FontStyle(self.font_style.clone())),
            (self.size != before.size).then_some(TextFormatChange::Size(self.size)),
            (self.tracking != before.tracking).then_some(TextFormatChange::Tracking(self.tracking)),
            (self.underline != before.underline).then_some(TextFormatChange::Underline(self.underline)),
            (self.strikethrough != before.strikethrough).then_some(TextFormatChange::Strikethrough(self.strikethrough)),
        ]
        .into_iter()
        .flatten()
    }
}
