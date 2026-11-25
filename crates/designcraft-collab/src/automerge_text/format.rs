use std::collections::BTreeMap;

use automerge::{
    AutoCommit, ObjId, ROOT, ReadDoc, ScalarValue,
    marks::{ExpandMark, Mark},
    transaction::Transactable,
};
use designcraft_doc::{BASIC_PARAGRAPH, CharFormat, NO_CHAR_STYLE, ParaFormat, Styles};
use serde_json::{Map, Value};

use crate::{
    Error,
    text::{BasicTextFormat, paragraph::ParagraphFormat},
};

pub(super) const CHARACTER: &str = "char:";
pub(super) const FOLLOWING_PARAGRAPH: &str = "next:";
pub(super) const FIRST_PARAGRAPH: &str = "first:";
pub(super) const EMPTY_CHARACTER: &str = "empty:";
const MAX_FORMAT_VALUE: usize = 1024;
const PARAGRAPH_GROUPS: [&str; 2] = ["para", "chars"];
const CHARACTER_PROPERTIES: &[&str] = &["style", "font_family", "font_style", "size", "tracking", "underline", "strikethrough"];
const PARAGRAPH_PROPERTIES: &[&str] = &[
    "style",
    "para/align",
    "para/leftIndent",
    "para/rightIndent",
    "para/firstLineIndent",
    "para/lastLineIndent",
    "para/spaceBefore",
    "para/spaceAfter",
    "para/hyphenate",
    "para/ragPattern",
    "para/ragInset",
    "para/ragLineInsets",
    "para/ragNearThreshold",
    "chars/language",
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Properties(BTreeMap<String, String>);

impl Properties {
    pub(super) fn characters(format: &CharFormat, styles: &Styles) -> Result<Self, Error> {
        let basic = BasicTextFormat::from_char_format(format)?;
        basic.validate(styles)?;
        let Value::Object(props) = serde_json::to_value(basic).map_err(|_| invalid_format())? else {
            return Err(invalid_format());
        };
        let mut properties = Self::default();
        for (key, value) in props {
            if !value.is_null() && (key != "style" || value != NO_CHAR_STYLE) {
                properties.put(key, &value)?;
            }
        }
        Ok(properties)
    }

    pub(super) fn paragraph(format: &ParaFormat, styles: &Styles) -> Result<Self, Error> {
        ParagraphFormat::from_doc(format, styles)?;
        let mut properties = Self::default();
        let Value::Object(props) = serde_json::to_value(format).map_err(|_| invalid_format())? else {
            return Err(invalid_format());
        };
        for (key, value) in props {
            if key == "style" && value != BASIC_PARAGRAPH {
                properties.put(key, &value)?;
            } else if PARAGRAPH_GROUPS.contains(&key.as_str()) {
                let Value::Object(fields) = value else { return Err(invalid_format()) };
                for (field, value) in fields {
                    properties.put(format!("{key}/{field}"), &value)?;
                }
            }
        }
        Ok(properties)
    }

    fn put(&mut self, key: String, value: &Value) -> Result<(), Error> {
        let value = serde_json::to_string(value).map_err(|_| invalid_format())?;
        if value.len() > MAX_FORMAT_VALUE {
            return Err(Error::Limit);
        }
        self.0.insert(key, value);
        Ok(())
    }

    fn read(&mut self, key: &str, value: &ScalarValue) -> Result<(), Error> {
        let value = value.as_str().filter(|value| value.len() <= MAX_FORMAT_VALUE).ok_or_else(invalid_format)?;
        self.0.insert(key.into(), value.into());
        Ok(())
    }

    pub(super) fn character_format(&self, styles: &Styles) -> Result<CharFormat, Error> {
        if self.is_empty() {
            return Ok(CharFormat::default());
        }
        let json = self.values()?;
        let basic: BasicTextFormat = serde_json::from_value(Value::Object(json)).map_err(|_| invalid_format())?;
        basic.validate(styles)?;
        let format = basic.to_char_format();
        if Self::characters(&format, styles)? != *self {
            return Err(invalid_format());
        }
        Ok(format)
    }

    pub(super) fn paragraph_format(&self, styles: &Styles) -> Result<ParaFormat, Error> {
        if self.is_empty() {
            return Ok(ParaFormat::default());
        }
        let mut format = Map::from_iter([("style".to_owned(), BASIC_PARAGRAPH.into())]);
        for (key, value) in self.values()? {
            if key == "style" {
                format.insert(key, value);
            } else {
                let (group, field) = key.split_once('/').filter(|(group, _)| PARAGRAPH_GROUPS.contains(group)).ok_or_else(invalid_format)?;
                let Value::Object(fields) = format.entry(group).or_insert_with(|| Value::Object(Map::new())) else {
                    return Err(invalid_format());
                };
                fields.insert(field.into(), value);
            }
        }
        let format = serde_json::from_value(Value::Object(format)).map_err(|_| invalid_format())?;
        if Self::paragraph(&format, styles)? != *self {
            return Err(invalid_format());
        }
        Ok(format)
    }

    fn values(&self) -> Result<Map<String, Value>, Error> {
        self.0.iter().map(|(key, value)| Ok((key.clone(), serde_json::from_str(value).map_err(|_| invalid_format())?))).collect()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn read_root(doc: &AutoCommit, prefix: &str) -> Result<Self, Error> {
        let mut properties = Self::default();
        for key in doc.keys(ROOT).filter(|key| key.starts_with(prefix)) {
            let value =
                doc.get(ROOT, &key).map_err(|_| invalid_format())?.and_then(|(value, _)| value.into_scalar().ok()).ok_or_else(invalid_format)?;
            properties.read(&key[prefix.len()..], &value)?;
        }
        Ok(properties)
    }

    pub(super) fn read_marks(marks: Option<&automerge::marks::MarkSet>) -> Result<(Self, Self), Error> {
        let mut characters = Self::default();
        let mut paragraph = Self::default();
        for (name, value) in marks.into_iter().flat_map(|marks| marks.iter()) {
            if let Some(key) = name.strip_prefix(CHARACTER) {
                characters.read(key, value)?;
            } else if let Some(key) = name.strip_prefix(FOLLOWING_PARAGRAPH) {
                paragraph.read(key, value)?;
            } else {
                return Err(invalid_format());
            }
        }
        Ok((characters, paragraph))
    }

    pub(super) fn write_root(&self, old: &Self, doc: &mut AutoCommit, prefix: &str) -> Result<(), Error> {
        for (key, value) in self.changed_from(old) {
            let key = format!("{prefix}{key}");
            if let Some(value) = value {
                doc.put(ROOT, key, value).map_err(|_| invalid_format())?;
            } else {
                doc.delete(ROOT, key).map_err(|_| invalid_format())?;
            }
        }
        Ok(())
    }

    pub(super) fn write_marks(&self, old: &Self, doc: &mut AutoCommit, object: &ObjId, prefix: &str, start: usize, end: usize) -> Result<(), Error> {
        for (key, value) in self.changed_from(old) {
            let key = format!("{prefix}{key}");
            if let Some(value) = value {
                doc.mark(object, Mark::new(key, value, start, end), ExpandMark::None).map_err(|_| invalid_format())?;
            } else {
                doc.unmark(object, &key, start, end, ExpandMark::None).map_err(|_| invalid_format())?;
            }
        }
        Ok(())
    }

    fn changed_from<'a>(&'a self, old: &'a Self) -> impl Iterator<Item = (&'a str, Option<&'a str>)> {
        self.0
            .iter()
            .filter(|(key, value)| old.0.get(*key) != Some(*value))
            .map(|(key, value)| (key.as_str(), Some(value.as_str())))
            .chain(old.0.keys().filter(|key| !self.0.contains_key(*key)).map(|key| (key.as_str(), None)))
    }
}

pub(super) fn validate_property(name: &str, value: Option<&ScalarValue>, prefix: &str, styles: &Styles) -> Result<(), Error> {
    let key = name.strip_prefix(prefix).ok_or_else(invalid_format)?;
    let character = prefix == CHARACTER || prefix == EMPTY_CHARACTER;
    let allowed = if character { CHARACTER_PROPERTIES } else { PARAGRAPH_PROPERTIES };
    if !allowed.contains(&key) {
        return Err(invalid_format());
    }
    if let Some(value) = value.filter(|value| !value.is_null()) {
        let mut properties = Properties::default();
        properties.read(key, value)?;
        if character {
            properties.character_format(styles)?;
        } else {
            properties.paragraph_format(styles)?;
        }
    }
    Ok(())
}

pub(super) fn invalid_format() -> Error {
    Error::Invalid("Automerge story formatting")
}
