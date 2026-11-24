use designcraft_doc::{Align, BASIC_PARAGRAPH, CharAttrs, MAX_MEASURE, ParaAttrs, ParaFormat, RagPattern, Styles};
use serde::{Deserialize, Serialize};

use super::{Error, validate_format_name};

/// Paragraph attributes whose native values can be represented exactly in the room.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ParagraphFormat {
    style: String,
    align: Option<Align>,
    left_indent: Option<f64>,
    right_indent: Option<f64>,
    first_line_indent: Option<f64>,
    last_line_indent: Option<f64>,
    space_before: Option<f64>,
    space_after: Option<f64>,
    hyphenate: Option<bool>,
    language: Option<String>,
    rag_pattern: Option<RagPattern>,
    rag_inset: Option<f64>,
    rag_line_insets: Option<[f64; 4]>,
    rag_near_threshold: Option<f64>,
}

impl ParagraphFormat {
    pub(super) fn from_doc(format: &ParaFormat, styles: &Styles) -> Result<Self, Error> {
        let attrs = &format.para;
        let shared = Self {
            style: format.style.clone(),
            align: attrs.align,
            left_indent: attrs.left_indent,
            right_indent: attrs.right_indent,
            first_line_indent: attrs.first_line_indent,
            last_line_indent: attrs.last_line_indent,
            space_before: attrs.space_before,
            space_after: attrs.space_after,
            hyphenate: attrs.hyphenate,
            language: format.chars.language.clone(),
            rag_pattern: attrs.rag_pattern,
            rag_inset: attrs.rag_inset,
            rag_line_insets: attrs.rag_line_insets,
            rag_near_threshold: attrs.rag_near_threshold,
        };
        resolve_style(styles, &shared.style)?;
        for change in [
            ParagraphFormatChange::LeftIndent(shared.left_indent),
            ParagraphFormatChange::RightIndent(shared.right_indent),
            ParagraphFormatChange::FirstLineIndent(shared.first_line_indent),
            ParagraphFormatChange::LastLineIndent(shared.last_line_indent),
            ParagraphFormatChange::SpaceBefore(shared.space_before),
            ParagraphFormatChange::SpaceAfter(shared.space_after),
            ParagraphFormatChange::RagInset(shared.rag_inset),
            ParagraphFormatChange::RagLineInsets(shared.rag_line_insets),
            ParagraphFormatChange::RagNearThreshold(shared.rag_near_threshold),
        ] {
            change.validate(styles)?;
        }
        validate_language(shared.language.as_deref())?;
        if shared.to_doc() != *format {
            return Err(Error::UnsupportedEdit);
        }
        Ok(shared)
    }

    pub(super) fn to_doc(&self) -> ParaFormat {
        ParaFormat {
            style: self.style.clone(),
            para: ParaAttrs {
                align: self.align,
                left_indent: self.left_indent,
                right_indent: self.right_indent,
                first_line_indent: self.first_line_indent,
                last_line_indent: self.last_line_indent,
                space_before: self.space_before,
                space_after: self.space_after,
                hyphenate: self.hyphenate,
                rag_pattern: self.rag_pattern,
                rag_inset: self.rag_inset,
                rag_line_insets: self.rag_line_insets,
                rag_near_threshold: self.rag_near_threshold,
                ..Default::default()
            },
            chars: CharAttrs { language: self.language.clone(), ..Default::default() },
            table: None,
        }
    }

    pub(super) fn changes(&self, before: &Self) -> impl Iterator<Item = ParagraphFormatChange> {
        [
            (self.style != before.style).then(|| ParagraphFormatChange::Style(self.style.clone())),
            (self.align != before.align).then_some(ParagraphFormatChange::Align(self.align)),
            (self.left_indent != before.left_indent).then_some(ParagraphFormatChange::LeftIndent(self.left_indent)),
            (self.right_indent != before.right_indent).then_some(ParagraphFormatChange::RightIndent(self.right_indent)),
            (self.first_line_indent != before.first_line_indent).then_some(ParagraphFormatChange::FirstLineIndent(self.first_line_indent)),
            (self.last_line_indent != before.last_line_indent).then_some(ParagraphFormatChange::LastLineIndent(self.last_line_indent)),
            (self.space_before != before.space_before).then_some(ParagraphFormatChange::SpaceBefore(self.space_before)),
            (self.space_after != before.space_after).then_some(ParagraphFormatChange::SpaceAfter(self.space_after)),
            (self.hyphenate != before.hyphenate).then_some(ParagraphFormatChange::Hyphenate(self.hyphenate)),
            (self.language != before.language).then(|| ParagraphFormatChange::Language(self.language.clone())),
            (self.rag_pattern != before.rag_pattern).then_some(ParagraphFormatChange::RagPattern(self.rag_pattern)),
            (self.rag_inset != before.rag_inset).then_some(ParagraphFormatChange::RagInset(self.rag_inset)),
            (self.rag_line_insets != before.rag_line_insets).then_some(ParagraphFormatChange::RagLineInsets(self.rag_line_insets)),
            (self.rag_near_threshold != before.rag_near_threshold).then_some(ParagraphFormatChange::RagNearThreshold(self.rag_near_threshold)),
        ]
        .into_iter()
        .flatten()
    }
}

/// One paragraph attribute write. `None` clears the local override.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value", deny_unknown_fields)]
pub enum ParagraphFormatChange {
    Style(String),
    Align(Option<Align>),
    LeftIndent(Option<f64>),
    RightIndent(Option<f64>),
    FirstLineIndent(Option<f64>),
    LastLineIndent(Option<f64>),
    SpaceBefore(Option<f64>),
    SpaceAfter(Option<f64>),
    Hyphenate(Option<bool>),
    RagPattern(Option<RagPattern>),
    RagInset(Option<f64>),
    Language(Option<String>),
    RagLineInsets(Option<[f64; 4]>),
    RagNearThreshold(Option<f64>),
}

impl ParagraphFormatChange {
    pub(super) fn validate(&self, styles: &Styles) -> Result<(), Error> {
        match self {
            Self::Style(name) => {
                validate_format_name(Some(name))?;
                resolve_style(styles, name)
            }
            Self::LeftIndent(value) => validate_measure(*value, 0.0, "left paragraph indent"),
            Self::RightIndent(value) => validate_measure(*value, 0.0, "right paragraph indent"),
            Self::FirstLineIndent(value) => validate_measure(*value, -MAX_MEASURE, "first-line indent"),
            Self::LastLineIndent(value) => validate_measure(*value, -MAX_MEASURE, "last-line indent"),
            Self::SpaceBefore(value) => validate_measure(*value, 0.0, "space before paragraph"),
            Self::SpaceAfter(value) => validate_measure(*value, 0.0, "space after paragraph"),
            Self::RagInset(value) => validate_measure(*value, 0.0, "short-line inset"),
            Self::Language(name) => validate_language(name.as_deref()),
            Self::RagLineInsets(values) => values.iter().flatten().try_for_each(|v| validate_measure(Some(*v), 0.0, "rag line inset")),
            Self::RagNearThreshold(value) => validate_measure(*value, 0.0, "near-flush threshold"),
            Self::Align(_) | Self::Hyphenate(_) | Self::RagPattern(_) => Ok(()),
        }
    }

    pub(super) fn apply(&self, format: &mut ParagraphFormat) {
        match self {
            Self::Style(value) => format.style.clone_from(value),
            Self::Align(value) => format.align = *value,
            Self::LeftIndent(value) => format.left_indent = *value,
            Self::RightIndent(value) => format.right_indent = *value,
            Self::FirstLineIndent(value) => format.first_line_indent = *value,
            Self::LastLineIndent(value) => format.last_line_indent = *value,
            Self::SpaceBefore(value) => format.space_before = *value,
            Self::SpaceAfter(value) => format.space_after = *value,
            Self::Hyphenate(value) => format.hyphenate = *value,
            Self::RagPattern(value) => format.rag_pattern = *value,
            Self::RagInset(value) => format.rag_inset = *value,
            Self::Language(value) => format.language.clone_from(value),
            Self::RagLineInsets(value) => format.rag_line_insets = *value,
            Self::RagNearThreshold(value) => format.rag_near_threshold = *value,
        }
    }
}

fn resolve_style(styles: &Styles, name: &str) -> Result<(), Error> {
    if name == BASIC_PARAGRAPH || styles.para(name).is_some() { Ok(()) } else { Err(Error::UnknownResource) }
}

fn validate_measure(value: Option<f64>, min: f64, field: &'static str) -> Result<(), Error> {
    if value.is_some_and(|value| !(min..=MAX_MEASURE).contains(&value)) { Err(Error::Invalid(field)) } else { Ok(()) }
}

fn validate_language(name: Option<&str>) -> Result<(), Error> {
    if name.is_some_and(|name| name.trim().is_empty()) {
        return Err(Error::Invalid("paragraph language"));
    }
    validate_format_name(name)
}
