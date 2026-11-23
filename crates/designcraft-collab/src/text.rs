//! Stable Unicode scalar identities for edits applied in the host's canonical order.
//!
//! The room authenticates the actor, orders and deduplicates its batches, and archives the seed and accepted operations for replay.
//! Text IDs are local to one seeded story. A new insertion follows its anchor immediately and precedes earlier insertions at
//! that exact anchor. Hidden atoms keep their payload and position so a batch can be undone without moving its anchors.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    sync::{Arc, OnceLock},
};

use designcraft_doc::{CharAttrs, CharFormat, CharRun, MAX_TYPE_SIZE, MIN_TYPE_SIZE, NO_CHAR_STYLE, ParaFormat, Story, StoryId, Styles};
use serde::{Deserialize, Serialize};

use crate::{Error, MAX_OPERATIONS, validation};

mod capture;
mod paragraph;
pub(crate) use capture::CaptureHint;
pub use capture::TextSplice;
use paragraph::ParagraphFormat;
pub use paragraph::ParagraphFormatChange;

/// Includes deleted anchors. Resetting the allowance requires a new seed and room epoch.
pub const MAX_STORY_ATOMS: usize = 16_384;
pub const MAX_EDIT_ATOMS: usize = 512;
const MAX_BATCH_ATOMS: usize = 1_024;
const MAX_STORY_BATCHES: usize = 1_024;
const MAX_FORMAT_NAME_BYTES: usize = 128;

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value", deny_unknown_fields)]
pub enum AtomId {
    Seed(u32),
    Inserted { actor: Arc<str>, sequence: u64, index: u16 },
}

/// The subset of character overrides that can cross the collaboration boundary.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BasicTextFormat {
    pub style: String,
    pub font_family: Option<String>,
    pub font_style: Option<String>,
    pub size: Option<f64>,
    pub tracking: Option<f64>,
    pub underline: Option<bool>,
    pub strikethrough: Option<bool>,
}

impl Default for BasicTextFormat {
    fn default() -> Self {
        Self { style: NO_CHAR_STYLE.into(), font_family: None, font_style: None, size: None, tracking: None, underline: None, strikethrough: None }
    }
}

impl BasicTextFormat {
    pub fn from_char_format(format: &CharFormat) -> Result<Self, Error> {
        let over = &format.over;
        let basic = Self {
            style: format.style.clone(),
            font_family: over.font_family.clone(),
            font_style: over.font_style.clone(),
            size: over.size,
            tracking: over.tracking,
            underline: over.underline,
            strikethrough: over.strikethrough,
        };
        if basic.to_char_format() != *format {
            return Err(Error::UnsupportedEdit);
        }
        Ok(basic)
    }

    pub fn to_char_format(&self) -> CharFormat {
        CharFormat {
            style: self.style.clone(),
            over: CharAttrs {
                font_family: self.font_family.clone(),
                font_style: self.font_style.clone(),
                size: self.size,
                tracking: self.tracking,
                underline: self.underline,
                strikethrough: self.strikethrough,
                ..Default::default()
            },
        }
    }

    fn validate(&self, styles: &Styles) -> Result<(), Error> {
        validate_character_style(styles, &self.style)?;
        validate_format_name(self.font_family.as_deref())?;
        validate_format_name(self.font_style.as_deref())?;
        validate_size(self.size)?;
        validate_tracking(self.tracking)
    }
}

/// Format one attribute at a time. Concurrent updates to distinct attributes compose. Competing updates to the same
/// attribute are resolved by the canonical order. `None` clears the corresponding local override.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value", deny_unknown_fields)]
pub enum TextFormatChange {
    Style(String),
    FontFamily(Option<String>),
    FontStyle(Option<String>),
    Size(Option<f64>),
    Tracking(Option<f64>),
    Underline(Option<bool>),
    Strikethrough(Option<bool>),
}

impl TextFormatChange {
    fn validate(&self, styles: &Styles) -> Result<(), Error> {
        match self {
            Self::Style(value) => validate_character_style(styles, value),
            Self::FontFamily(value) | Self::FontStyle(value) => validate_format_name(value.as_deref()),
            Self::Size(value) => validate_size(*value),
            Self::Tracking(value) => validate_tracking(*value),
            Self::Underline(_) | Self::Strikethrough(_) => Ok(()),
        }
    }

    fn apply(&self, format: &mut BasicTextFormat) {
        match self {
            Self::Style(value) => format.style.clone_from(value),
            Self::FontFamily(value) => format.font_family.clone_from(value),
            Self::FontStyle(value) => format.font_style.clone_from(value),
            Self::Size(value) => format.size = *value,
            Self::Tracking(value) => format.tracking = *value,
            Self::Underline(value) => format.underline = *value,
            Self::Strikethrough(value) => format.strikethrough = *value,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
pub enum TextEdit {
    Insert {
        after: Option<AtomId>,
        text: String,
        format: BasicTextFormat,
    },
    Delete {
        atoms: Vec<AtomId>,
    },
    Format {
        atoms: Vec<AtomId>,
        change: TextFormatChange,
    },
    /// A paragraph is named by its preceding newline, or by the start of the story.
    FormatParagraph {
        after: Option<AtomId>,
        change: ParagraphFormatChange,
    },
}

impl TextEdit {
    pub fn inserted_atom_count(&self) -> usize {
        match self {
            Self::Insert { text, .. } => text.chars().count(),
            _ => 0,
        }
    }
}

#[derive(Clone)]
struct Character {
    scalar: char,
    format: Arc<BasicTextFormat>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct EditPosition {
    batch: usize,
    index: usize,
}

#[derive(Clone)]
struct FormatWrite {
    position: EditPosition,
    change: Arc<TextFormatChange>,
}

#[derive(Clone)]
struct Paragraph {
    base: Arc<ParagraphFormat>,
    current: Arc<ParagraphFormat>,
    writes: Vec<(usize, ParagraphFormatChange)>,
}

impl Paragraph {
    fn new(base: Arc<ParagraphFormat>) -> Self {
        Self { current: base.clone(), base, writes: Vec::new() }
    }

    fn write(&mut self, batch: usize, change: &ParagraphFormatChange) {
        change.apply(Arc::make_mut(&mut self.current));
        self.writes.push((batch, change.clone()));
    }

    fn update_active(paragraph: &mut Arc<Self>, batch: usize, active: &[bool]) {
        if !paragraph.writes.iter().any(|(index, _)| *index == batch) {
            return;
        }
        let paragraph = Arc::make_mut(paragraph);
        let mut current = paragraph.base.clone();
        for (batch, change) in &paragraph.writes {
            if active[*batch] {
                change.apply(Arc::make_mut(&mut current));
            }
        }
        paragraph.current = current;
    }
}

struct StoryProjection {
    text: String,
    chars: Vec<CharRun>,
    paras: Vec<ParaFormat>,
}

#[derive(Clone)]
struct EmptySample {
    position: EditPosition,
    first_visible: AtomId,
}

#[derive(Clone)]
struct Atom {
    id: AtomId,
    character: Character,
    base_format: Arc<BasicTextFormat>,
    insertion: Option<usize>,
    deletions: Option<Arc<Vec<usize>>>,
    formats: Option<Arc<Vec<FormatWrite>>>,
    next_paragraph: Option<Arc<Paragraph>>,
}

impl Atom {
    fn new(id: AtomId, scalar: char, format: Arc<BasicTextFormat>, insertion: Option<usize>) -> Self {
        Self {
            id,
            character: Character { scalar, format: format.clone() },
            base_format: format,
            insertion,
            deletions: None,
            formats: None,
            next_paragraph: None,
        }
    }

    fn is_visible(&self, active: &[bool]) -> bool {
        self.insertion.is_none_or(|batch| active[batch]) && !self.deletions.as_deref().is_some_and(|tags| tags.iter().any(|&batch| active[batch]))
    }

    fn active_format(&self, active: &[bool], before: Option<EditPosition>) -> Arc<BasicTextFormat> {
        let mut format = self.base_format.clone();
        for write in self.formats.as_deref().into_iter().flatten() {
            if before.is_some_and(|end| write.position >= end) {
                break;
            }
            if active[write.position.batch] {
                write.change.apply(Arc::make_mut(&mut format));
            }
        }
        format
    }
}

/// Identity and basic formatting for a single simple upstream story.
#[derive(Clone)]
pub struct TextReplica {
    id: StoryId,
    first_paragraph: Arc<Paragraph>,
    seed_typing_format: Arc<BasicTextFormat>,
    empty_samples: Vec<EmptySample>,
    batches: BTreeMap<Arc<str>, BTreeMap<u64, usize>>,
    active: Vec<bool>,
    atoms: Vec<Atom>,
    visible_offsets: Arc<OnceLock<BTreeMap<AtomId, Range<usize>>>>,
}

impl TextReplica {
    pub fn new(story: &Story, styles: &Styles) -> Result<Self, Error> {
        validate_story_shape(story)?;
        let mut paragraphs = story.paras.iter().map(|format| ParagraphFormat::from_doc(format, styles).map(Arc::new));
        let first_paragraph = Arc::new(Paragraph::new(paragraphs.next().ok_or(Error::Invalid("story paragraphs"))??));
        let first = story.chars.first().ok_or(Error::Invalid("story runs"))?;
        let typing_format = if story.text.is_empty() { BasicTextFormat::from_char_format(&first.format)? } else { BasicTextFormat::default() };
        let mut atoms = Vec::new();
        let mut offset: usize = 0;
        for run in &story.chars {
            let end = offset.checked_add(run.len).ok_or(Error::Invalid("story runs"))?;
            let segment = story.text.get(offset..end).ok_or(Error::Invalid("story run boundaries"))?;
            if segment.is_empty() && (!story.text.is_empty() || story.chars.len() != 1) {
                return Err(Error::Invalid("story runs"));
            }
            let format = BasicTextFormat::from_char_format(&run.format)?;
            format.validate(styles)?;
            let format = Arc::new(format);
            for scalar in segment.chars() {
                if atoms.len() == MAX_STORY_ATOMS {
                    return Err(Error::Limit);
                }
                let mut atom = Atom::new(AtomId::Seed(atoms.len() as u32), scalar, format.clone(), None);
                if scalar == '\n' {
                    atom.next_paragraph = Some(Arc::new(Paragraph::new(paragraphs.next().ok_or(Error::Invalid("story paragraphs"))??)));
                }
                atoms.push(atom);
            }
            offset = end;
        }
        if offset != story.text.len() {
            return Err(Error::Invalid("story runs"));
        }
        Ok(Self {
            id: story.id,
            first_paragraph,
            seed_typing_format: Arc::new(typing_format),
            empty_samples: Vec::new(),
            batches: Default::default(),
            active: Vec::new(),
            atoms,
            visible_offsets: Default::default(),
        })
    }

    /// Convert a current UTF-8 byte boundary to its stable predecessor. `None` is the start of the story.
    pub fn atom_before(&self, byte: usize) -> Result<Option<AtomId>, Error> {
        let mut offset = 0;
        let mut before = None;
        for (id, character) in self.visible() {
            if offset == byte {
                return Ok(before);
            }
            offset += character.scalar.len_utf8();
            before = Some(id.clone());
        }
        if offset == byte { Ok(before) } else { Err(Error::Invalid("text byte offset")) }
    }

    /// Resolve an anchor to a visible byte boundary, including a deleted anchor's retained position.
    pub fn byte_after(&self, anchor: &AtomId) -> Result<usize, Error> {
        let mut offset = 0;
        for atom in &self.atoms {
            if atom.is_visible(&self.active) {
                offset += atom.character.scalar.len_utf8();
            }
            if &atom.id == anchor {
                return Ok(offset);
            }
        }
        Err(Error::Invalid("text anchor"))
    }

    /// Capture only the atoms visible in a valid UTF-8 byte range. The IDs remain usable after remote insertions.
    pub fn atoms_in(&self, range: Range<usize>) -> Result<Vec<AtomId>, Error> {
        if range.start > range.end {
            return Err(Error::Invalid("text range"));
        }
        self.atom_before(range.start)?;
        self.atom_before(range.end)?;
        let mut offset = 0;
        let mut ids = Vec::new();
        for (id, character) in self.visible() {
            if offset >= range.end {
                break;
            }
            if offset >= range.start {
                if ids.len() == MAX_EDIT_ATOMS {
                    return Err(Error::Limit);
                }
                ids.push(id.clone());
            }
            offset += character.scalar.len_utf8();
        }
        Ok(ids)
    }

    /// Check story order across all atoms, including those already deleted when the comment arrives.
    pub(crate) fn validate_comment_atoms(&self, ids: &[AtomId]) -> Result<(), Error> {
        if ids.len() > MAX_EDIT_ATOMS {
            return Err(Error::Limit);
        }
        let mut ordered_atoms = self.atoms.iter();
        if ids.is_empty() || !ids.iter().all(|id| ordered_atoms.any(|atom| &atom.id == id)) {
            return Err(Error::Invalid("text comment range"));
        }
        Ok(())
    }

    pub(crate) fn visible_anchored_range(&self, ids: &[AtomId]) -> Option<Range<usize>> {
        let offsets = self.visible_offsets.get_or_init(|| {
            let mut start = 0;
            self.visible()
                .map(|(id, character)| {
                    let end = start + character.scalar.len_utf8();
                    let range = start..end;
                    start = end;
                    (id.clone(), range)
                })
                .collect()
        });
        let mut visible = ids.iter().filter_map(|id| offsets.get(id));
        let first = visible.next()?;
        let end = visible.next_back().map_or(first.end, |last| last.end);
        Some(first.start..end)
    }

    /// Apply an accepted canonical batch atomically to the current story. Inserted IDs use the batch's authenticated
    /// actor and sequence, with indices starting at zero across all insertions in that batch.
    pub fn apply(&mut self, story: &mut Story, styles: &Styles, actor: &str, sequence: u64, edits: &[TextEdit]) -> Result<(), Error> {
        validation::actor(actor)?;
        if sequence == 0 {
            return Err(Error::Invalid("proposal sequence"));
        }
        if edits.len() > MAX_OPERATIONS {
            return Err(Error::Limit);
        }
        if edits.is_empty() {
            return Ok(());
        }
        let before = self.projection();
        self.check_projection(story, &before)?;
        if self.batch_index(actor, sequence).is_some() {
            return Err(Error::DuplicateId);
        }
        if self.active.len() == MAX_STORY_BATCHES {
            return Err(Error::Limit);
        }
        let actor: Arc<str> = actor.into();
        let mut staged = self.clone();
        let batch = staged.active.len();
        staged.active.push(true);
        staged.batches.entry(actor.clone()).or_default().insert(sequence, batch);
        staged.apply_edits(styles, actor, sequence, batch, edits)?;
        staged.project_into(story, &before);
        staged.visible_offsets = Default::default();
        *self = staged;
        Ok(())
    }

    pub fn can_set_batch_active(&self, actor: &str, sequence: u64, active: bool) -> bool {
        self.batch_index(actor, sequence).is_some_and(|index| self.active[index] != active)
    }

    /// Toggle an accepted batch in its original position without changing the IDs it contributed.
    pub fn set_batch_active(&mut self, story: &mut Story, actor: &str, original_sequence: u64, active: bool) -> Result<(), Error> {
        let index = self.batch_index(actor, original_sequence).ok_or(Error::Invalid("text history batch"))?;
        if self.active[index] == active {
            return Err(Error::Invalid("text history state"));
        }
        let before = self.projection();
        self.check_projection(story, &before)?;
        self.active[index] = active;
        Paragraph::update_active(&mut self.first_paragraph, index, &self.active);
        for atom in &mut self.atoms {
            if let Some(paragraph) = &mut atom.next_paragraph {
                Paragraph::update_active(paragraph, index, &self.active);
            }
            if atom.formats.as_deref().is_some_and(|writes| writes.iter().any(|write| write.position.batch == index)) {
                atom.character.format = atom.active_format(&self.active, None);
            }
        }
        self.project_into(story, &before);
        self.visible_offsets = Default::default();
        Ok(())
    }

    fn batch_index(&self, actor: &str, sequence: u64) -> Option<usize> {
        self.batches.get(actor)?.get(&sequence).copied()
    }

    fn check_projection(&self, story: &Story, projected: &StoryProjection) -> Result<(), Error> {
        validate_story_shape(story)?;
        if story.id != self.id
            || story.text != projected.text
            || story.paras != projected.paras
            || !runs_match(&projected.text, &story.chars, &projected.chars)
        {
            return Err(Error::Invalid("story projection"));
        }
        Ok(())
    }

    fn apply_edits(&mut self, styles: &Styles, actor: Arc<str>, sequence: u64, batch: usize, edits: &[TextEdit]) -> Result<(), Error> {
        let mut offset = 0;
        let mut touched = 0;
        for (index, edit) in edits.iter().enumerate() {
            touched += edit_count(edit)?;
            if touched > MAX_BATCH_ATOMS {
                return Err(Error::Limit);
            }
            let position = EditPosition { batch, index };
            match edit {
                TextEdit::Insert { after, text, format } => {
                    format.validate(styles)?;
                    let chars = text.chars().enumerate().map(|(index, scalar)| {
                        let id = AtomId::Inserted { actor: actor.clone(), sequence, index: (offset + index) as u16 };
                        (id, scalar)
                    });
                    self.insert(after.as_ref(), chars, Arc::new(format.clone()), batch)?;
                    offset += text.chars().count();
                }
                TextEdit::Delete { atoms } => self.delete(atoms, position)?,
                TextEdit::Format { atoms, change } => {
                    change.validate(styles)?;
                    let write = FormatWrite { position, change: Arc::new(change.clone()) };
                    let active = &self.active;
                    Self::update(&mut self.atoms, atoms, |atom| {
                        if atom.is_visible(active) {
                            change.apply(Arc::make_mut(&mut atom.character.format));
                            Arc::make_mut(atom.formats.get_or_insert_with(Default::default)).push(write.clone());
                        }
                    })?;
                }
                TextEdit::FormatParagraph { after, change } => {
                    change.validate(styles)?;
                    let paragraph = match after {
                        Some(anchor) => self
                            .atoms
                            .iter_mut()
                            .find(|atom| &atom.id == anchor)
                            .and_then(|atom| atom.next_paragraph.as_mut())
                            .ok_or(Error::Invalid("paragraph anchor"))?,
                        None => &mut self.first_paragraph,
                    };
                    Arc::make_mut(paragraph).write(batch, change);
                }
            }
        }
        Ok(())
    }

    fn insert(
        &mut self,
        after: Option<&AtomId>,
        chars: impl Iterator<Item = (AtomId, char)>,
        format: Arc<BasicTextFormat>,
        batch: usize,
    ) -> Result<(), Error> {
        let position = match after {
            Some(anchor) => self.atoms.iter().position(|atom| &atom.id == anchor).ok_or(Error::Invalid("text anchor"))? + 1,
            None => 0,
        };
        let mut inherited = None;
        let atoms: Vec<_> = chars
            .map(|(id, scalar)| {
                let mut atom = Atom::new(id, scalar, format.clone(), Some(batch));
                if scalar == '\n' {
                    let paragraph = inherited.get_or_insert_with(|| {
                        let parent = self.atoms[..position]
                            .iter()
                            .rev()
                            .filter(|atom| atom.is_visible(&self.active))
                            .find_map(|atom| atom.next_paragraph.as_ref())
                            .unwrap_or(&self.first_paragraph);
                        Arc::new(Paragraph::new(parent.current.clone()))
                    });
                    atom.next_paragraph = Some(paragraph.clone());
                }
                atom
            })
            .collect();
        if self.atoms.len() + atoms.len() > MAX_STORY_ATOMS {
            return Err(Error::Limit);
        }
        self.atoms.splice(position..position, atoms);
        Ok(())
    }

    fn update(atoms: &mut [Atom], targets: &[AtomId], mut change: impl FnMut(&mut Atom)) -> Result<(), Error> {
        let ids: BTreeSet<_> = targets.iter().collect();
        if ids.len() != targets.len() {
            return Err(Error::Invalid("duplicate text targets"));
        }
        let mut found = 0;
        for atom in atoms {
            if ids.contains(&atom.id) {
                change(atom);
                found += 1;
            }
        }
        if found == ids.len() { Ok(()) } else { Err(Error::Invalid("text target")) }
    }

    fn delete(&mut self, targets: &[AtomId], position: EditPosition) -> Result<(), Error> {
        let first_visible = self.visible().next().map(|(id, _)| id.clone());
        Self::update(&mut self.atoms, targets, |atom| {
            let tags = atom.deletions.get_or_insert_with(Default::default);
            if tags.last() != Some(&position.batch) {
                Arc::make_mut(tags).push(position.batch);
            }
        })?;
        if let Some(first_visible) = first_visible
            && self.visible().next().is_none()
        {
            self.empty_samples.push(EmptySample { position, first_visible });
        }
        Ok(())
    }

    fn visible(&self) -> impl Iterator<Item = (&AtomId, &Character)> {
        self.atoms.iter().filter(|atom| atom.is_visible(&self.active)).map(|atom| (&atom.id, &atom.character))
    }

    fn visible_paragraphs(&self) -> impl Iterator<Item = (Option<&AtomId>, &Paragraph)> {
        std::iter::once((None, self.first_paragraph.as_ref())).chain(self.atoms.iter().filter_map(|atom| {
            if !atom.is_visible(&self.active) {
                return None;
            }
            atom.next_paragraph.as_deref().map(|paragraph| (Some(&atom.id), paragraph))
        }))
    }

    fn projection(&self) -> StoryProjection {
        let mut text = String::new();
        let mut runs: Vec<CharRun> = Vec::new();
        let mut previous = None;
        for (_, character) in self.visible() {
            text.push(character.scalar);
            let length = character.scalar.len_utf8();
            let format = character.format.as_ref();
            if previous == Some(format)
                && let Some(run) = runs.last_mut()
            {
                run.len += length;
            } else {
                runs.push(CharRun { len: length, format: format.to_char_format() });
            }
            previous = Some(format);
        }
        if runs.is_empty() {
            runs.push(CharRun { len: 0, format: self.empty_format().to_char_format() });
        }
        let paras = self.visible_paragraphs().map(|(_, paragraph)| paragraph.current.to_doc()).collect();
        StoryProjection { text, chars: runs, paras }
    }

    fn empty_format(&self) -> Arc<BasicTextFormat> {
        self.empty_samples
            .iter()
            .rev()
            .find(|sample| self.active[sample.position.batch])
            .and_then(|sample| {
                self.atoms.iter().find(|atom| atom.id == sample.first_visible).map(|atom| atom.active_format(&self.active, Some(sample.position)))
            })
            .unwrap_or_else(|| self.seed_typing_format.clone())
    }

    fn project_into(&self, story: &mut Story, before: &StoryProjection) {
        let StoryProjection { text, chars, paras } = self.projection();
        if text != before.text || chars != before.chars || paras != before.paras {
            story.text = text;
            story.chars = chars;
            story.paras = paras;
            story.rev = story.rev.wrapping_add(1);
        }
    }
}

fn validate_story_shape(story: &Story) -> Result<(), Error> {
    if story.text.len() > MAX_STORY_ATOMS * 4 || story.chars.len() > MAX_STORY_ATOMS || story.paras.len() > MAX_STORY_ATOMS + 1 {
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
        || !simple_text(&story.text)
    {
        return Err(Error::UnsupportedEdit);
    }
    if story.paras.len() != story.text.bytes().filter(|&byte| byte == b'\n').count() + 1 {
        return Err(Error::Invalid("story paragraphs"));
    }
    Ok(())
}

pub(crate) fn simple_text(text: &str) -> bool {
    text.chars().all(|ch| matches!(ch, '\n' | '\t') || (!ch.is_control() && !matches!(ch, '\u{2028}' | '\u{e000}'..='\u{f8ff}' | '\u{fffc}')))
}

fn edit_count(edit: &TextEdit) -> Result<usize, Error> {
    let length = match edit {
        TextEdit::Insert { text, .. } => {
            if text.len() > MAX_EDIT_ATOMS * 4 {
                return Err(Error::Limit);
            }
            if !simple_text(text) {
                return Err(Error::UnsupportedEdit);
            }
            text.chars().count()
        }
        TextEdit::Delete { atoms } | TextEdit::Format { atoms, .. } => atoms.len(),
        TextEdit::FormatParagraph { .. } => 1,
    };
    match length {
        0 => Err(Error::Invalid("empty text edit")),
        1..=MAX_EDIT_ATOMS => Ok(length),
        _ => Err(Error::Limit),
    }
}

fn runs_match(text: &str, source: &[CharRun], canonical: &[CharRun]) -> bool {
    let mut source = source.iter();
    let mut offset = 0;
    for expected in canonical {
        let mut remaining = expected.len;
        loop {
            let Some(actual) = source.next() else { return false };
            if actual.format != expected.format || actual.len > remaining || actual.len == 0 && expected.len != 0 {
                return false;
            }
            offset += actual.len;
            if !text.is_char_boundary(offset) {
                return false;
            }
            remaining -= actual.len;
            if remaining == 0 {
                break;
            }
        }
    }
    source.next().is_none()
}

fn validate_character_style(styles: &Styles, name: &str) -> Result<(), Error> {
    validate_format_name(Some(name))?;
    if name == NO_CHAR_STYLE || styles.char_style(name).is_some() { Ok(()) } else { Err(Error::UnknownResource) }
}

fn validate_format_name(name: Option<&str>) -> Result<(), Error> {
    if name.is_some_and(|name| name.len() > MAX_FORMAT_NAME_BYTES || name.chars().any(char::is_control)) {
        Err(Error::Invalid("text format name"))
    } else {
        Ok(())
    }
}

fn validate_size(value: Option<f64>) -> Result<(), Error> {
    if value.is_some_and(|value| !(MIN_TYPE_SIZE..=MAX_TYPE_SIZE).contains(&value)) { Err(Error::Invalid("character size")) } else { Ok(()) }
}

fn validate_tracking(value: Option<f64>) -> Result<(), Error> {
    if value.is_some_and(|value| !(-1_000.0..=10_000.0).contains(&value)) { Err(Error::Invalid("character tracking")) } else { Ok(()) }
}

#[cfg(test)]
mod tests;
