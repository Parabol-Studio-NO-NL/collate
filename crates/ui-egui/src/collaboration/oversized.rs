//! Images larger than a room shares, and reduced copies of them that fit.
//!
//! A room bounds every image so each peer can decode it. Rather than refuse a
//! document, sharing names these images and replaces them with smaller copies:
//! in a separate shared copy when a room starts, and as a placement is shared
//! during a session. The placed size on the page does not change.

use std::sync::Arc;

use designcraft_collab::artwork::{MAX_ARTWORK_BYTES, MAX_IMAGE_PIXELS, MAX_IMAGE_SIDE, exceeds_room_limit};
use designcraft_doc::{Asset, AssetData, AssetId, Document};

/// The room limit as people read it.
pub(super) const LIMIT: &str = "Rooms share images up to 16 megapixels.";

#[derive(Clone, Debug, PartialEq)]
pub(super) struct OversizedImage {
    pub id: AssetId,
    pub name: String,
    pub pixels: (u32, u32),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ReducedImage {
    pub name: String,
    pub from: (u32, u32),
    pub to: (u32, u32),
}

fn oversized(asset: &Asset) -> Option<OversizedImage> {
    let pixels = asset.data.embedded_bytes().and_then(|bytes| exceeds_room_limit(&asset.mime, bytes)).or_else(|| {
        // A linked original is read when the shared copy is saved; its recorded size names it first.
        let pixels = asset.pixels.filter(|_| asset.data.embedded_bytes().is_none())?;
        let area = u64::from(pixels.0) * u64::from(pixels.1);
        (designcraft_collab::artwork::raster_extension(&asset.mime).is_some() && (area > MAX_IMAGE_PIXELS || pixels.0.max(pixels.1) > MAX_IMAGE_SIDE))
            .then_some(pixels)
    })?;
    Some(OversizedImage { id: asset.id, name: display_name(asset), pixels })
}

fn display_name(asset: &Asset) -> String {
    if !asset.name.trim().is_empty() {
        return asset.name.clone();
    }
    asset.link.as_deref().and_then(|link| link.rsplit(['/', '\\']).next()).filter(|name| !name.is_empty()).unwrap_or("Untitled image").to_owned()
}

pub(super) fn is_oversized(asset: &Asset) -> bool {
    oversized(asset).is_some()
}

/// Every image in `doc` that is too large for a room, largest first.
pub(super) fn oversized_images(doc: &Document) -> Vec<OversizedImage> {
    let mut images: Vec<_> = doc.assets.values().filter_map(|asset| oversized(asset)).collect();
    images.sort_by_key(|image| std::cmp::Reverse(u64::from(image.pixels.0) * u64::from(image.pixels.1)));
    images
}

/// A line that names an image and its size, for a message that cannot list a dialog row.
pub(super) fn describe(image: &OversizedImage) -> String {
    format!("“{}” · {} × {} px", image.name, image.pixels.0, image.pixels.1)
}

/// A room error naming each image that does not fit. Each line is translated on its own,
/// so the first stays a fixed message.
pub(super) fn too_large_message(images: &[OversizedImage]) -> String {
    let mut message = String::from(if images.len() == 1 {
        "This image is larger than a room can share."
    } else {
        "These images are larger than a room can share."
    });
    for image in images.iter().take(5) {
        message.push('\n');
        message.push_str(&describe(image));
    }
    message.push('\n');
    message.push_str(LIMIT);
    message
}

/// The room error for one asset whose recorded or probed size does not fit.
pub(super) fn too_large_asset(asset: &Asset, pixels: (u32, u32)) -> String {
    too_large_message(&[OversizedImage { id: asset.id, name: display_name(asset), pixels }])
}

/// Refuse a document with oversized images, naming all of them.
pub(super) fn check_document(doc: &Document) -> Result<(), String> {
    let images = oversized_images(doc);
    if images.is_empty() { Ok(()) } else { Err(too_large_message(&images)) }
}

/// Replace each oversized embedded image chosen by `include` with a reduced copy.
pub(super) fn reduce_images(doc: &mut Document, include: impl Fn(&Asset) -> bool) -> Result<Vec<ReducedImage>, String> {
    let ids: Vec<_> = doc.assets.values().filter(|asset| include(asset)).filter_map(|asset| oversized(asset)).collect();
    let mut reduced = Vec::new();
    for image in ids {
        let Some(asset) = doc.assets.get_mut(&image.id) else { continue };
        let to = reduce_asset(Arc::make_mut(asset))
            .ok_or_else(|| format!("Collate could not make a smaller copy of this image.\n{}\n{LIMIT}", describe(&image)))?;
        reduced.push(ReducedImage { name: image.name, from: image.pixels, to });
    }
    Ok(reduced)
}

fn reduce_asset(asset: &mut Asset) -> Option<(u32, u32)> {
    let bytes = asset.data.embedded_bytes()?;
    let image = designcraft_render::proxy::reduce_to_fit(bytes, MAX_IMAGE_SIDE, MAX_IMAGE_PIXELS, MAX_ARTWORK_BYTES)?;
    asset.mime = image.mime.to_owned();
    asset.data = AssetData::embedded(image.data);
    asset.pixels = Some(image.pixels);
    // The bytes no longer match any original file.
    asset.set_link(None, None);
    asset.link_relative = None;
    asset.link_hash = None;
    Some(image.pixels)
}

/// The notice after reducing images, one line per image.
pub(super) fn reduced_message(images: &[ReducedImage]) -> Option<String> {
    if images.is_empty() {
        return None;
    }
    let mut message = String::from(if images.len() == 1 {
        "Shared a smaller copy of an image that was larger than the room allows."
    } else {
        "Shared smaller copies of images that were larger than the room allows."
    });
    for image in images.iter().take(5) {
        message.push('\n');
        message.push_str(&format!("“{}” · {} × {} → {} × {} px", image.name, image.from.0, image.from.1, image.to.0, image.to.1));
    }
    Some(message)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use craft_image_codecs::{ChannelLayout, Format, Image};
    use designcraft_collab::RoomProtocol;
    use designcraft_doc::{AssetData, Content, Document, Selection};
    use designcraft_engine::DeferredEdit;

    use super::super::{reduce_new_images, seed_document};
    use super::{MAX_IMAGE_SIDE, oversized_images, reduce_images};

    /// One pixel wider than a room allows, and only two pixels tall.
    fn too_wide_png() -> Vec<u8> {
        let width = MAX_IMAGE_SIDE + 1;
        let image = Image::from_u8(width, 2, ChannelLayout::Rgba, [40, 90, 160, 255].repeat(width as usize * 2)).unwrap();
        craft_image_codecs::encode(&image, Format::Png, &Default::default()).unwrap()
    }

    fn with_oversized_image() -> (Document, designcraft_doc::AssetId, designcraft_doc::ItemId) {
        let (mut doc, asset, item) = seed_document::embedded_image_fixture();
        let entry = Arc::make_mut(doc.assets.get_mut(&asset).unwrap());
        entry.name = "Harbour panorama.png".into();
        entry.data = AssetData::embedded(too_wide_png());
        entry.pixels = Some((MAX_IMAGE_SIDE + 1, 2));
        (doc, asset, item)
    }

    fn placed_size(doc: &Document, item: designcraft_doc::ItemId) -> (f64, f64) {
        let loc = doc.find(item).unwrap();
        match &doc.item_at(&loc).unwrap().content {
            Content::Graphic(graphic) => graphic.size,
            _ => panic!("the fixture places a graphic"),
        }
    }

    #[test]
    fn a_room_names_the_image_it_cannot_share() {
        let (doc, ..) = with_oversized_image();
        let error = seed_document::check_artwork_for_room(&doc, RoomProtocol::Automerge).unwrap_err();
        assert!(error.contains("Harbour panorama.png"), "{error}");
        assert!(error.contains(&format!("{} × 2", MAX_IMAGE_SIDE + 1)), "{error}");
    }

    #[test]
    fn a_reduced_copy_fits_the_room_at_the_same_placed_size() {
        let (mut doc, asset, item) = with_oversized_image();
        let size = placed_size(&doc, item);
        let reduced = reduce_images(&mut doc, |_| true).unwrap();
        assert_eq!(reduced.len(), 1);
        assert!(reduced[0].to.0 < reduced[0].from.0);
        assert!(oversized_images(&doc).is_empty());
        seed_document::check_artwork_for_room(&doc, RoomProtocol::Automerge).unwrap();
        assert_eq!(doc.assets[&asset].pixels, Some(reduced[0].to));
        assert_eq!(placed_size(&doc, item), size);
    }

    #[test]
    fn a_live_edit_reduces_only_artwork_it_adds() {
        let (oversized, asset, _) = with_oversized_image();
        let (mut before, ..) = seed_document::embedded_image_fixture();
        let layer = before.default_layer();
        // The edit replaces the small image with a large one.
        let mut edit = DeferredEdit {
            before: Arc::new(before.clone()),
            after: Arc::new(oversized.clone()),
            before_selection: Selection::default(),
            before_layer: layer,
            after_selection: Selection::default(),
        };
        let reduced = reduce_new_images(&mut edit).unwrap();
        assert_eq!(reduced.len(), 1);
        assert!(oversized_images(&edit.after).is_empty());

        // An image already in the room is left for the room to report, not changed silently.
        before.assets = oversized.assets.clone();
        let mut unchanged = DeferredEdit { before: Arc::new(before), after: Arc::new(oversized), ..edit };
        assert!(reduce_new_images(&mut unchanged).unwrap().is_empty());
        assert_eq!(unchanged.after.assets[&asset].pixels, Some((MAX_IMAGE_SIDE + 1, 2)));
    }
}
