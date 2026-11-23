//! Native layer changes in the host's document order. Allocated layer IDs are
//! shared because every room replica replays the same allocations from one seed.

use std::{collections::BTreeSet, sync::Arc};

use designcraft_doc::{Document, Item, ItemId, Layer, LayerId, MAX_NEXT_ID};
use serde::{Deserialize, Serialize};

use crate::Error;

const MAX_LAYERS: usize = 256;
const MAX_NAME_BYTES: usize = 128;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum LayerEdit {
    /// The native layer's draft ID is replaced by the next confirmed allocation.
    Create {
        layer: Layer,
        before: Option<LayerId>,
    },
    Set {
        layers: Vec<Layer>,
    },
    Reorder {
        order: Vec<LayerId>,
    },
    Remove {
        layers: Vec<LayerId>,
        into: Option<LayerId>,
    },
}

impl LayerEdit {
    pub(crate) fn conflicts(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Set { layers: left }, Self::Set { layers: right }) => left.iter().any(|layer| right.iter().any(|other| other.id == layer.id)),
            (Self::Create { .. }, Self::Create { .. } | Self::Set { .. })
            | (Self::Set { .. }, Self::Create { .. } | Self::Reorder { .. })
            | (Self::Reorder { .. }, Self::Set { .. }) => false,
            _ => true,
        }
    }

    pub(crate) fn conflicts_with_content(&self) -> bool {
        matches!(self, Self::Set { .. } | Self::Remove { .. })
    }
}

pub(crate) fn capture(before: &Document, after: &Document) -> Result<Option<LayerEdit>, Error> {
    if before.layers == after.layers {
        return Ok(None);
    }
    let previous = layer_ids(before);
    let current = layer_ids(after);
    let removed = previous.difference(&current).copied().collect::<Vec<_>>();
    if let Some((index, layer)) = after.layers.iter().enumerate().find(|(_, layer)| !previous.contains(&layer.id)) {
        if !removed.is_empty() || current.len() != previous.len() + 1 {
            return Err(Error::UnsupportedEdit);
        }
        let before = after.layers.get(index + 1).map(|next| next.id);
        return Ok(Some(LayerEdit::Create { layer: layer.clone(), before }));
    }
    if !removed.is_empty() {
        let into = before
            .spreads
            .iter()
            .chain(&before.parents)
            .flat_map(|spread| &spread.items)
            .filter(|item| !current.contains(&item.layer))
            .find_map(|item| after.item(item.id).map(|retained| retained.layer));
        return Ok(Some(LayerEdit::Remove { layers: removed, into }));
    }
    let order = after.layers.iter().map(|layer| layer.id).collect::<Vec<_>>();
    if before.layers.iter().map(|layer| layer.id).ne(order.iter().copied()) {
        return Ok(Some(LayerEdit::Reorder { order }));
    }
    let layers = before.layers.iter().zip(&after.layers).filter(|(old, new)| old != new).map(|(_, new)| new.clone()).collect();
    Ok(Some(LayerEdit::Set { layers }))
}

/// Mutate a staged document and return IDs removed from its regular spreads.
pub(crate) fn apply(document: &mut Document, edit: &LayerEdit) -> Result<Vec<ItemId>, Error> {
    match edit {
        LayerEdit::Create { layer, before } => create(document, layer, *before)?,
        LayerEdit::Set { layers } => set(document, layers)?,
        LayerEdit::Reorder { order } => reorder(document, order)?,
        LayerEdit::Remove { layers, into } => return remove(document, layers, *into),
    }
    Ok(Vec::new())
}

fn layer_ids(document: &Document) -> BTreeSet<LayerId> {
    document.layers.iter().map(|layer| layer.id).collect()
}

fn bounded_ids(ids: &[LayerId]) -> Result<BTreeSet<LayerId>, Error> {
    if ids.is_empty() || ids.len() > MAX_LAYERS {
        return Err(Error::Limit);
    }
    let distinct: BTreeSet<_> = ids.iter().copied().collect();
    if distinct.len() != ids.len() {
        return Err(Error::DuplicateId);
    }
    Ok(distinct)
}

fn validate_name(name: &str) -> Result<(), Error> {
    if name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(Error::Invalid("layer name"));
    }
    Ok(())
}

fn create(document: &mut Document, layer: &Layer, before: Option<LayerId>) -> Result<(), Error> {
    if document.layers.len() >= MAX_LAYERS {
        return Err(Error::Limit);
    }
    validate_name(&layer.name)?;
    let index = match before {
        Some(before) => document.layers.iter().position(|layer| layer.id == before).ok_or(Error::UnknownResource)?,
        None => document.layers.len(),
    };
    let id = LayerId(document.next_id.checked_add(1).filter(|next| *next <= MAX_NEXT_ID).ok_or(Error::Limit)?);
    if document.layer(id).is_some() {
        return Err(Error::DuplicateId);
    }
    document.next_id = id.0;
    document.layers.insert(index, Layer { id, ..layer.clone() });
    Ok(())
}

fn set(document: &mut Document, layers: &[Layer]) -> Result<(), Error> {
    let ids = layers.iter().map(|layer| layer.id).collect::<Vec<_>>();
    bounded_ids(&ids)?;
    for layer in layers {
        let old = document.layer(layer.id).ok_or(Error::UnknownResource)?;
        if layer.name != old.name {
            validate_name(&layer.name)?;
            if layer.name.is_empty()
                || layer.name.trim() != layer.name
                || document.layers.iter().any(|other| other.id != layer.id && other.name == layer.name)
            {
                return Err(Error::Invalid("layer name"));
            }
        }
        *document.layer_mut(layer.id).ok_or(Error::UnknownResource)? = layer.clone();
    }
    Ok(())
}

fn reorder(document: &mut Document, order: &[LayerId]) -> Result<(), Error> {
    if bounded_ids(order)? != layer_ids(document) || order.len() != document.layers.len() {
        return Err(Error::Invalid("layer order"));
    }
    document.layers = order.iter().map(|id| document.layer(*id).cloned().ok_or(Error::UnknownResource)).collect::<Result<_, _>>()?;
    Ok(())
}

fn remove(document: &mut Document, layers: &[LayerId], into: Option<LayerId>) -> Result<Vec<ItemId>, Error> {
    let removed = bounded_ids(layers)?;
    let catalog = layer_ids(document);
    if !removed.is_subset(&catalog) || removed == catalog || into.is_some_and(|id| !catalog.contains(&id) || removed.contains(&id)) {
        return Err(Error::Invalid("layer removal"));
    }
    validate_references(document, &removed)?;
    let deleted = match into {
        Some(target) => {
            for spread in document.spreads.iter_mut().chain(&mut document.parents) {
                if spread.items.iter().any(|item| removed.contains(&item.layer)) {
                    for item in &mut Arc::make_mut(spread).items {
                        if removed.contains(&item.layer) {
                            Arc::make_mut(item).layer = target;
                        }
                    }
                }
            }
            Vec::new()
        }
        None => delete_layer_frames(document, &removed)?,
    };
    document.layers.retain(|layer| !removed.contains(&layer.id));
    Ok(deleted)
}

fn validate_references(document: &Document, removed: &BTreeSet<LayerId>) -> Result<(), Error> {
    if document.anchored_items().iter().any(|(_, item)| removed.contains(&item.layer)) {
        return Err(Error::UnsupportedObject);
    }
    for spread in document.spreads.iter().chain(&document.parents) {
        if spread.pages.iter().flat_map(|page| &page.guides).any(|guide| guide.layer.is_some_and(|layer| removed.contains(&layer))) {
            return Err(Error::UnsupportedEdit);
        }
        for item in &spread.items {
            let mut child_uses_layer = false;
            item.walk(&mut |child| child_uses_layer |= child.id != item.id && removed.contains(&child.layer));
            if child_uses_layer {
                return Err(Error::UnsupportedObject);
            }
        }
    }
    Ok(())
}

fn delete_layer_frames(document: &mut Document, removed: &BTreeSet<LayerId>) -> Result<Vec<ItemId>, Error> {
    if document.parents.iter().flat_map(|spread| &spread.items).any(|item| removed.contains(&item.layer)) {
        return Err(Error::UnsupportedObject);
    }
    let ids =
        document.spreads.iter().flat_map(|spread| &spread.items).filter(|item| removed.contains(&item.layer)).map(|item| item.id).collect::<Vec<_>>();
    if !ids.is_empty() && has_inbound_frame_reference(document, &ids) {
        return Err(Error::UnsupportedObject);
    }
    for id in &ids {
        document.remove_item(*id).map_err(|_| Error::UnsupportedObject)?;
    }
    Ok(ids)
}

fn has_inbound_frame_reference(document: &Document, ids: &[ItemId]) -> bool {
    let removed: BTreeSet<_> = ids.iter().copied().collect();
    let refers_to_removed = |item: &Item| {
        !removed.contains(&item.id)
            && (item.overrides.is_some_and(|id| removed.contains(&id))
                || item.live_caption.as_ref().is_some_and(|caption| removed.contains(&caption.source)))
    };
    for spread in document.spreads.iter().chain(&document.parents) {
        for item in &spread.items {
            let mut referenced = false;
            item.walk(&mut |child| referenced |= refers_to_removed(child));
            if referenced {
                return true;
            }
        }
    }
    document.anchored_items().iter().any(|(_, item)| refers_to_removed(item))
}
