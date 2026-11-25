//! Bounded native seed bundles with exact object IDs and embedded graphics.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::sync::{Arc, Weak};

#[cfg(test)]
use craft_image_codecs::Format;
use craft_zip::{Archive, Compression, Limits, Writer, preflight};
use designcraft_collab::artwork::{
    BlobId, MAX_ARTWORK_BYTES, MAX_IMAGE_PIXELS, PDF_MIME, pdf_dimensions, raster_dimensions as image_dimensions, raster_format as image_format,
    valid_dimensions,
};
use designcraft_doc::{Asset, AssetData, AssetId, Document, Storage, StoryId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{attachments::Blobs, seed::MAX_SEED_BYTES};

pub(super) const MAX_MODEL_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ASSETS: usize = 128;
const MAX_PROXY_BYTES: u64 = 1024 * 1024;
const MAX_PROXY_PIXELS: u64 = 1024 * 1024;
const MAX_TOTAL_PIXELS: u64 = 32 * 1024 * 1024;
// The native File > New page size ceiling, in points (216 inches).
const MAX_PAGE_POINTS: f64 = 15_552.0;
const NATIVE_MEMBERS: [&str; 3] = ["mimetype", "meta.json", "document.json"];
const INVALID: &str = "The received document bundle is invalid.";
const TOO_LARGE: &str = "The shared document exceeds the room size limit.";
const UNSUPPORTED_LINKS: &str = "Shared documents cannot contain linked assets, linked stories or data merges.";
const UNSUPPORTED_ASSET: &str = "Shared documents support embedded PNG, JPEG, GIF, WebP and PDF artwork.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    format: String,
    version: u32,
    #[serde(rename = "generator")]
    _generator: String,
}

struct ModelBytes(Vec<u8>);

impl Write for ModelBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_MODEL_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other(TOO_LARGE));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn model_bytes(doc: &Document) -> Result<Vec<u8>, String> {
    let mut output = ModelBytes(Vec::new());
    serde_json::to_writer(&mut output, doc).map_err(|error| if error.is_io() { TOO_LARGE } else { "Could not encode the shared document." })?;
    Ok(output.0)
}

#[derive(Default)]
struct JsonSize(usize);

impl Write for JsonSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).filter(|&len| len <= MAX_MODEL_BYTES).ok_or_else(|| io::Error::other(TOO_LARGE))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn model_size(value: &impl Serialize) -> Result<usize, String> {
    let mut output = JsonSize::default();
    serde_json::to_writer(&mut output, value).map_err(|error| if error.is_io() { TOO_LARGE } else { "Could not encode the shared document." })?;
    Ok(output.0)
}

/// A text-only commit replaces story values under their unchanged map keys. Validate the
/// full document limit by counting only those values, including their escaped JSON text.
pub(super) fn updated_story_model_size(
    original_size: usize,
    before: &Document,
    after: &Document,
    stories: impl Iterator<Item = StoryId>,
) -> Result<usize, String> {
    let mut removed = 0;
    let mut added = 0;
    for id in stories {
        removed += model_size(before.story(id).ok_or(INVALID)?)?;
        added += model_size(after.story(id).ok_or(INVALID)?)?;
    }
    let size = original_size.checked_sub(removed).ok_or(INVALID)? + added;
    if size > MAX_MODEL_BYTES {
        return Err(TOO_LARGE.into());
    }
    Ok(size)
}

/// A weak owner pins the allocation identity without retaining the artwork's
/// bytes. Replacing the data, even with the same length, gets a distinct entry.
#[derive(Default)]
pub(super) struct Fingerprints {
    digests: BTreeMap<*const Vec<u8>, (Weak<Vec<u8>>, [u8; 32])>,
    #[cfg(test)]
    pub(super) bytes_hashed: u64,
}

impl Fingerprints {
    fn digest(&mut self, body: &Arc<Vec<u8>>) -> [u8; 32] {
        self.digests
            .entry(Arc::as_ptr(body))
            .or_insert_with(|| {
                #[cfg(test)]
                {
                    self.bytes_hashed += body.len() as u64;
                }
                (Arc::downgrade(body), Sha256::digest(body.as_slice()).into())
            })
            .1
    }

    pub(super) fn canonical(&mut self, doc: &Document) -> Result<[u8; 32], String> {
        let mut canonical = doc.clone();
        canonical.compact_assets_with_hash(|asset| {
            asset.data.embedded_bytes().map_or_else(
                || asset.data.key().0,
                |body| {
                    let digest = self.digest(body);
                    u64::from_be_bytes(std::array::from_fn(|i| digest[i]))
                },
            )
        });
        self.fingerprint(&canonical)
    }

    fn fingerprint(&mut self, doc: &Document) -> Result<[u8; 32], String> {
        validate_model(doc)?;
        let payloads = payloads(doc, false)?;
        let mut normalized = doc.clone();
        normalized.created = 0;
        normalized.modified = 0;
        normalized.font_scope = 0;
        let model = model_bytes(&normalized)?;
        let mut fingerprint = Sha256::new();
        fingerprint.update(b"CoDesign native seed\0");
        fingerprint.update((model.len() as u64).to_be_bytes());
        fingerprint.update(model);
        for (name, data) in payloads {
            fingerprint.update((name.len() as u64).to_be_bytes());
            fingerprint.update(name.as_bytes());
            fingerprint.update(self.digest(data));
        }
        Ok(fingerprint.finalize().into())
    }
}

#[cfg(test)]
pub(super) fn fingerprint(doc: &Document) -> Result<[u8; 32], String> {
    Fingerprints::default().fingerprint(doc)
}

const BUNDLE_MAGIC: &[u8] = b"Collate shared assets v1\n";
type Members = BTreeMap<AssetId, (BlobId, Option<BlobId>)>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest<D> {
    document: D,
    members: Members,
}

pub(super) fn artwork_size(doc: &Document) -> u64 {
    doc.assets
        .values()
        .flat_map(|asset| std::iter::once(&asset.data).chain(asset.proxy.iter().map(|p| &p.data)))
        .filter_map(AssetData::embedded_bytes)
        .map(|body| body.len() as u64)
        .sum()
}

pub(super) fn bundle(doc: &Document) -> Result<(Vec<u8>, Blobs), String> {
    let size = artwork_size(doc);
    if size <= (MAX_SEED_BYTES / 2) as u64 {
        return Ok((encode(doc)?, Blobs::new()));
    }
    if size > MAX_ARTWORK_BYTES as u64 {
        return Err(TOO_LARGE.into());
    }
    validate_model(doc)?;
    payloads(doc, true)?;
    let mut blobs = Blobs::new();
    let mut identify = |data: &AssetData| -> Result<BlobId, String> {
        let body = data.embedded_bytes().ok_or(UNSUPPORTED_LINKS)?;
        let id = BlobId { length: u32::try_from(body.len()).map_err(|_| TOO_LARGE)?, digest: Sha256::digest(body.as_slice()).into() };
        blobs.entry(id).or_insert_with(|| body.clone());
        Ok(id)
    };
    let mut members = Members::new();
    for (&id, asset) in &doc.assets {
        members.insert(id, (identify(&asset.data)?, asset.proxy.as_ref().map(|p| identify(&p.data)).transpose()?));
    }
    let mut output = ModelBytes(BUNDLE_MAGIC.into());
    serde_json::to_writer(&mut output, &Manifest { document: doc, members }).map_err(|_| TOO_LARGE)?;
    Ok((output.0, blobs))
}

/// The loader must authenticate the supplied digest. Reject all metadata and length
/// violations before opening any files or allocating memory for artwork.
pub(super) fn restore_bundle(bytes: &[u8], mut load: impl FnMut(BlobId) -> Result<Arc<Vec<u8>>, String>) -> Result<Document, String> {
    if bytes.len() > MAX_SEED_BYTES {
        return Err(TOO_LARGE.into());
    }
    let Some(manifest) = bytes.strip_prefix(BUNDLE_MAGIC) else { return decode(bytes) };
    let Manifest { mut document, members }: Manifest<Document> = serde_json::from_slice(manifest).map_err(|_| INVALID)?;
    validate_model(&document)?;
    if document.assets.len() != members.len() {
        return Err(INVALID.into());
    }
    let mut total = 0u64;
    for (&id, asset) in &document.assets {
        let &(body, proxy) = members.get(&id).ok_or(INVALID)?;
        if !body.valid() || asset.proxy.is_some() != proxy.is_some() || proxy.is_some_and(|p| !p.valid() || u64::from(p.length) > MAX_PROXY_BYTES) {
            return Err(INVALID.into());
        }
        total += u64::from(body.length) + proxy.map_or(0, |p| u64::from(p.length));
    }
    if total > MAX_ARTWORK_BYTES as u64 {
        return Err(TOO_LARGE.into());
    }
    let mut loaded = Blobs::new();
    let mut resolve = |id: BlobId| -> Result<AssetData, String> {
        if let std::collections::btree_map::Entry::Vacant(entry) = loaded.entry(id) {
            let data = load(id)?;
            if data.len() != id.length as usize {
                return Err(INVALID.into());
            }
            entry.insert(data);
        }
        Ok(AssetData::embedded(loaded[&id].clone()))
    };
    for (id, (body, preview)) in members {
        let asset = Arc::make_mut(document.assets.get_mut(&id).ok_or(INVALID)?);
        asset.data = resolve(body)?;
        if let (Some(proxy), Some(preview)) = (&mut asset.proxy, preview) {
            proxy.data = resolve(preview)?;
        }
    }
    payloads(&document, true)?;
    Ok(document)
}

pub(super) fn encode(doc: &Document) -> Result<Vec<u8>, String> {
    validate_model(doc)?;
    let payloads = payloads(doc, true)?;
    let model = model_bytes(doc)?;
    let metadata = serde_json::to_vec(&serde_json::json!({
        "format": "designcraft",
        "version": designcraft_format::VERSION,
        "generator": "Collate Collaboration"
    }))
    .map_err(|_| "Could not encode the shared document.")?;
    if expanded_size(&payloads) + (model.len() + metadata.len() + designcraft_format::MIME.len()) as u64 > MAX_EXPANDED_BYTES {
        return Err(TOO_LARGE.into());
    }
    // The normal save path compacts/remaps asset IDs, and normal load can
    // repair old metadata. A live replica must keep the exact seed IDs.
    let mut archive = Writer::new();
    archive.add("mimetype", designcraft_format::MIME.as_bytes(), Compression::Stored).map_err(|_| INVALID)?;
    archive.add("meta.json", &metadata, Compression::Deflated).map_err(|_| INVALID)?;
    archive.add("document.json", &model, Compression::Deflated).map_err(|_| INVALID)?;
    for (name, data) in payloads {
        archive.add(&name, data, Compression::Stored).map_err(|_| INVALID)?;
    }
    let bytes = archive.finish().map_err(|_| INVALID)?;
    if bytes.len() > MAX_SEED_BYTES {
        return Err(TOO_LARGE.into());
    }
    Ok(bytes)
}

pub(super) fn decode(bytes: &[u8]) -> Result<Document, String> {
    if bytes.is_empty() || bytes.len() > MAX_SEED_BYTES {
        return Err(TOO_LARGE.into());
    }
    let max_entries = (NATIVE_MEMBERS.len() + 2 * MAX_ASSETS) as u64;
    let directory = preflight::preflight(bytes, preflight::Limits { max_entries, max_central_directory_bytes: 128 * 1024, max_name_bytes: 80 })
        .map_err(|_| INVALID)?;
    let mut archive = Archive::open(bytes, Limits { max_entries, max_total_bytes: MAX_EXPANDED_BYTES }).map_err(|_| INVALID)?;
    if archive.has_duplicate_names() || directory.entries != archive.len() as u64 || archive.has_overlapping_files().map_err(|_| INVALID)? {
        return Err(INVALID.into());
    }
    validate_container(&mut archive)?;

    let model = member(&mut archive, "document.json", MAX_MODEL_BYTES as u64)?;
    let mut doc: Document = serde_json::from_slice(&model).map_err(|_| INVALID)?;
    validate_model(&doc)?;
    let mut expected: BTreeSet<_> = NATIVE_MEMBERS.into_iter().map(str::to_owned).collect();
    for asset in doc.assets.values() {
        expected.insert(asset_name("assets", asset.id.0, &asset.mime)?);
        if let Some(proxy) = &asset.proxy {
            expected.insert(asset_name("proxies", asset.id.0, &proxy.mime)?);
        }
    }
    if expected.len() != archive.len() || !archive.names().all(|name| expected.contains(name.as_ref())) {
        return Err(INVALID.into());
    }
    for asset in doc.assets.values_mut() {
        let asset = Arc::make_mut(asset);
        let name = asset_name("assets", asset.id.0, &asset.mime)?;
        let limit = if asset.mime == PDF_MIME { MAX_ARTWORK_BYTES } else { MAX_SEED_BYTES };
        asset.data = AssetData::embedded(member(&mut archive, &name, limit as u64)?);
        if let Some(proxy) = &mut asset.proxy {
            let name = asset_name("proxies", asset.id.0, &proxy.mime)?;
            proxy.data = AssetData::embedded(member(&mut archive, &name, MAX_PROXY_BYTES)?);
        }
    }
    payloads(&doc, true)?;
    Ok(doc)
}

fn validate_container(archive: &mut Archive<'_>) -> Result<(), String> {
    for index in 0..archive.len() {
        if archive.entry_is_dir(index).map_err(|_| INVALID)? != Some(false) || archive.entry_is_symlink(index).map_err(|_| INVALID)? != Some(false) {
            return Err(INVALID.into());
        }
    }
    let mime = member(archive, "mimetype", designcraft_format::MIME.len() as u64)?;
    if mime != designcraft_format::MIME.as_bytes() {
        return Err(INVALID.into());
    }
    let meta: Metadata = serde_json::from_slice(&member(archive, "meta.json", 1024)?).map_err(|_| INVALID)?;
    if meta.format != "designcraft" || meta.version != designcraft_format::VERSION {
        return Err("The seed needs a compatible native document version.".into());
    }
    Ok(())
}

fn member(archive: &mut Archive<'_>, name: &str, limit: u64) -> Result<Vec<u8>, String> {
    archive.read(name, limit).map_err(|_| INVALID)?.ok_or_else(|| INVALID.into())
}

fn has_external_bytes(asset: &Asset) -> bool {
    asset.storage == Storage::Linked || asset.data.is_linked() || asset.proxy.as_ref().is_some_and(|proxy| proxy.data.is_linked())
}

fn has_link_provenance(asset: &Asset) -> bool {
    asset.link.is_some() || asset.link_relative.is_some() || asset.link_hash.is_some() || asset.link_fingerprint.is_some()
}

/// Embed Link keeps the source location for a future local Unembed. Remove it only
/// from a local working copy with the original bytes in hand, before compaction or
/// comparison with the room. Incoming seeds must pass strict validation as received.
pub(super) fn prepare_local_copy(doc: &mut Document) -> Result<(), String> {
    for asset in doc.assets.values_mut() {
        if has_external_bytes(asset) {
            return Err(UNSUPPORTED_LINKS.into());
        }
        if has_link_provenance(asset) {
            if asset.data.embedded_bytes().is_none_or(|body| body.is_empty()) {
                return Err(UNSUPPORTED_LINKS.into());
            }
            let asset = Arc::make_mut(asset);
            asset.set_link(None, None);
            asset.link_relative = None;
            asset.link_hash = None;
        }
    }
    Ok(())
}

fn validate_model(doc: &Document) -> Result<(), String> {
    if doc.assets.len() > MAX_ASSETS {
        return Err(TOO_LARGE.into());
    }
    if !valid_layout(doc)
        || doc.placed_items().into_iter().any(|item| !item.geometry_is_finite())
        || doc.check().is_err()
        || !doc.used_assets().iter().all(|id| doc.assets.contains_key(id))
    {
        return Err(INVALID.into());
    }
    if !doc.data_merge.is_empty() || has_linked_story(doc) {
        return Err(UNSUPPORTED_LINKS.into());
    }
    for (&id, asset) in &doc.assets {
        if id != asset.id {
            return Err(INVALID.into());
        }
        if has_external_bytes(asset) || has_link_provenance(asset) {
            return Err(UNSUPPORTED_LINKS.into());
        }
        validate_asset_metadata(asset)?;
    }
    Ok(())
}

fn valid_layout(doc: &Document) -> bool {
    let size = |width, height| width > 0.0 && width <= MAX_PAGE_POINTS && height > 0.0 && height <= MAX_PAGE_POINTS;
    if doc.spreads.is_empty() || doc.layers.is_empty() || !size(doc.settings.page_width, doc.settings.page_height) {
        return false;
    }
    let mut spreads = BTreeSet::new();
    let mut pages = BTreeSet::new();
    let mut layers = BTreeSet::new();
    doc.layers.iter().all(|layer| layers.insert(layer.id))
        && doc.spreads.iter().chain(&doc.parents).all(|spread| {
            spreads.insert(spread.id)
                && !spread.pages.is_empty()
                && spread
                    .pages
                    .iter()
                    .all(|page| pages.insert(page.id) && size(page.width, page.height) && page.x.abs() <= 10_000.0 * MAX_PAGE_POINTS)
        })
}

fn validate_asset_metadata(asset: &Asset) -> Result<(), String> {
    if asset.mime != PDF_MIME {
        image_format(&asset.mime).ok_or(UNSUPPORTED_ASSET)?;
    }
    if asset.pixels.is_some_and(|pixels| !valid_dimensions(pixels, MAX_IMAGE_PIXELS)) {
        return Err("A shared image exceeds the room pixel limit.".into());
    }
    if let Some(proxy) = &asset.proxy
        && (!matches!(proxy.mime.as_str(), "image/png" | "image/jpeg") || !valid_dimensions(proxy.pixels, MAX_PROXY_PIXELS))
    {
        return Err("A shared image preview is invalid.".into());
    }
    Ok(())
}

fn has_linked_story(doc: &Document) -> bool {
    let mut stories: Vec<_> = doc.stories.values().map(Arc::as_ref).collect();
    while let Some(story) = stories.pop() {
        if story.file_link.is_some() || story.link.is_some() {
            return true;
        }
        stories.extend(story.notes.iter().chain(&story.endnotes).map(|note| &note.text));
        stories.extend(story.tables.values().flat_map(|table| table.cells.iter().map(|cell| &cell.text)));
    }
    false
}

fn asset_name(directory: &str, id: u64, mime: &str) -> Result<String, String> {
    let extension = if directory == "assets" && mime == PDF_MIME { "pdf" } else { image_format(mime).ok_or(UNSUPPORTED_ASSET)?.1 };
    Ok(format!("{directory}/{id}.{extension}"))
}

fn payloads(doc: &Document, verify_image_body: bool) -> Result<Vec<(String, &Arc<Vec<u8>>)>, String> {
    let mut entries = Vec::new();
    let mut pixels = 0u64;
    for asset in doc.assets.values() {
        let data = asset.data.embedded_bytes().ok_or(UNSUPPORTED_LINKS)?;
        if data.len() > MAX_ARTWORK_BYTES {
            return Err(TOO_LARGE.into());
        }
        let remaining = MAX_IMAGE_PIXELS.min(MAX_TOTAL_PIXELS - pixels);
        let dimensions = if asset.mime == PDF_MIME {
            let dimensions = pdf_dimensions(asset, data, verify_image_body)?;
            if !valid_dimensions(dimensions, remaining) {
                return Err(TOO_LARGE.into());
            }
            dimensions
        } else {
            image_dimensions(&asset.mime, data, remaining, verify_image_body)?
        };
        pixels += u64::from(dimensions.0) * u64::from(dimensions.1);
        entries.push((asset_name("assets", asset.id.0, &asset.mime)?, data));

        if let Some(proxy) = &asset.proxy {
            let data = proxy.data.embedded_bytes().ok_or(UNSUPPORTED_LINKS)?;
            if data.len() as u64 > MAX_PROXY_BYTES {
                return Err(TOO_LARGE.into());
            }
            let dims = image_dimensions(&proxy.mime, data, MAX_PROXY_PIXELS.min(MAX_TOTAL_PIXELS - pixels), verify_image_body)?;
            if dims != proxy.pixels {
                return Err("A shared image preview is invalid.".into());
            }
            pixels += u64::from(dims.0) * u64::from(dims.1);
            entries.push((asset_name("proxies", asset.id.0, &proxy.mime)?, data));
        }
    }
    if expanded_size(&entries) > MAX_ARTWORK_BYTES as u64 {
        return Err(TOO_LARGE.into());
    }
    Ok(entries)
}

pub(super) fn check_artwork(doc: &Document) -> Result<(), String> {
    if doc.assets.len() > MAX_ASSETS {
        return Err(TOO_LARGE.into());
    }
    payloads(doc, false).map(|_| ())
}

fn expanded_size(entries: &[(String, &Arc<Vec<u8>>)]) -> u64 {
    entries.iter().map(|(_, data)| data.len() as u64).sum()
}

#[cfg(test)]
pub(super) fn embedded_image_fixture() -> (Document, designcraft_doc::AssetId, designcraft_doc::ItemId) {
    use designcraft_doc::build::NewDocument;
    use designcraft_doc::geom::{Affine, Rect, shapes};
    use designcraft_doc::{AssetId, Content, Graphic, Item, ItemId, Proxy, Shape, SpreadRef};

    let mut doc = Document::new(&NewDocument::default());
    let asset = AssetId(doc.alloc());
    let used = ItemId(doc.alloc());
    doc.assets.insert(
        asset,
        Arc::new(Asset {
            id: asset,
            name: "Synthetic colour chip".into(),
            mime: "image/png".into(),
            data: AssetData::embedded(test_png(20)),
            pixels: Some((1, 1)),
            proxy: Some(Proxy { mime: "image/png".into(), pixels: (1, 1), data: AssetData::embedded(test_png(30)) }),
            ..Default::default()
        }),
    );
    let mut frame = Item::new(used, doc.default_layer(), Shape::Rectangle, shapes::rectangle(Rect::new(0.0, 0.0, 20.0, 20.0)));
    frame.content = Content::Graphic(Graphic {
        asset,
        size: (1.0, 1.0),
        xf: Affine::IDENTITY,
        layer_options: None,
        clip: None,
        auto_fit: Default::default(),
        fit_align: 4,
        crop: [0.0; 4],
        wrap: Default::default(),
        fill: Default::default(),
    });
    doc.insert_item(SpreadRef::Doc(0), frame, None).unwrap();
    (doc, asset, used)
}

#[cfg(test)]
fn test_png(red: u8) -> Vec<u8> {
    let raster = craft_image_codecs::Image::from_u8(1, 1, craft_image_codecs::ChannelLayout::Rgba, vec![red, 0, 0, 255]).unwrap();
    craft_image_codecs::encode(&raster, Format::Png, &Default::default()).unwrap()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use craft_zip::{Archive, Compression, Limits, Writer};
    use designcraft_doc::{AssetData, AssetId, Storage};

    use super::{
        MAX_EXPANDED_BYTES, MAX_MODEL_BYTES, MAX_SEED_BYTES, UNSUPPORTED_LINKS, decode, embedded_image_fixture, encode, fingerprint, model_bytes,
        test_png,
    };

    #[test]
    fn text_only_model_accounting_matches_the_native_document_size() {
        use designcraft_doc::{Document, SpreadRef, build::NewDocument, geom::Rect};

        let mut before = Document::new(&NewDocument::default());
        let layer = before.default_layer();
        let bounds = Rect::new(20.0, 20.0, 100.0, 100.0);
        let (_, opening) = before.add_text_frame(SpreadRef::Doc(0), bounds, layer, "Opening", Default::default()).unwrap();
        let (_, closing) = before.add_text_frame(SpreadRef::Doc(0), bounds, layer, "Remove this", Default::default()).unwrap();
        let original = model_bytes(&before).unwrap().len();
        let mut after = before.clone();
        after.story_mut(opening).unwrap().insert(0, "\"Café\"\n\t🌿\\\n");
        after.story_mut(closing).unwrap().delete(0.."Remove this".len());
        let expected = model_bytes(&after).unwrap().len();
        assert_eq!(super::updated_story_model_size(original, &before, &after, [opening, closing].into_iter()), Ok(expected));
        assert!(expected > original);
        assert!(super::updated_story_model_size(MAX_MODEL_BYTES, &before, &after, [opening, closing].into_iter()).is_err());
    }

    fn with_members(bytes: &[u8], extras: &[(&str, &[u8])], omit: Option<&str>) -> Vec<u8> {
        let mut source = Archive::open(bytes, Limits { max_entries: 10, max_total_bytes: MAX_EXPANDED_BYTES }).unwrap();
        let names: Vec<_> = source.names().map(|name| name.into_owned()).collect();
        let mut archive = Writer::new();
        for name in names {
            if omit == Some(name.as_str()) {
                continue;
            }
            let data = source.read(&name, MAX_MODEL_BYTES as u64).unwrap().unwrap();
            archive.add(&name, &data, Compression::Stored).unwrap();
        }
        for &(path, bytes) in extras {
            archive.add(path, bytes, Compression::Deflated).unwrap();
        }
        archive.finish().unwrap()
    }

    #[test]
    fn round_trip_preserves_native_ids_exact_pixels_and_proxy_bytes() {
        let (mut doc, asset, item) = embedded_image_fixture();
        let duplicate = AssetId(doc.alloc());
        let mut unused = doc.assets[&asset].as_ref().clone();
        unused.id = duplicate;
        doc.assets.insert(duplicate, Arc::new(unused));
        let encoded = encode(&doc).unwrap();
        let received = decode(&encoded).unwrap();
        assert_eq!(received, doc);
        assert_eq!(received.item(item).unwrap().graphic().unwrap().asset, asset);
        assert!(designcraft_format::load(&encoded).is_ok());
        assert_eq!(fingerprint(&received), fingerprint(&doc));
    }

    #[test]
    fn fingerprint_excludes_times_and_local_font_scope_but_includes_exact_asset_bytes() {
        let (mut doc, asset, _) = embedded_image_fixture();
        let before = fingerprint(&doc).unwrap();
        doc.created += 1;
        doc.modified += 9;
        doc.font_scope += 3;
        assert_eq!(fingerprint(&doc).unwrap(), before);
        Arc::make_mut(doc.assets.get_mut(&asset).unwrap()).data = AssetData::embedded(test_png(44));
        assert_ne!(fingerprint(&doc).unwrap(), before);
    }

    #[test]
    fn invalid_member_inventory_and_overexpansion_are_rejected() {
        let (doc, asset, _) = embedded_image_fixture();
        let archive = encode(&doc).unwrap();
        let asset_path = format!("assets/{}.png", asset.0);
        assert!(decode(&with_members(&archive, &[], Some(&asset_path))).is_err());
        assert!(decode(&with_members(&archive, &[("assets/../outside", b"synthetic")], None)).is_err());
        let oversized_model = vec![b' '; MAX_MODEL_BYTES + 1];
        let too_big = with_members(&archive, &[("document.json", &oversized_model)], Some("document.json"));
        assert!(too_big.len() < MAX_SEED_BYTES);
        assert!(decode(&too_big).is_err());

        // Both local and central ZIP records hold the name. Changing the
        // spelling after write produces the duplicate a normal writer rejects.
        let mut duplicate = with_members(&archive, &[("document.jsoX", b"{}")], None);
        let locations: Vec<_> =
            duplicate.windows(b"document.jsoX".len()).enumerate().filter_map(|(index, bytes)| (bytes == b"document.jsoX").then_some(index)).collect();
        assert_eq!(locations.len(), 2);
        for start in locations {
            duplicate[start..start + b"document.json".len()].copy_from_slice(b"document.json");
        }
        assert!(decode(&duplicate).is_err());
    }

    #[test]
    fn linked_sources_and_invalid_raster_bodies_are_rejected() {
        let (mut doc, asset, _) = embedded_image_fixture();
        let encoded = encode(&doc).unwrap();
        let path = format!("assets/{}.png", asset.0);
        let truncated_png = &test_png(77)[..24];
        assert!(decode(&with_members(&encoded, &[(&path, truncated_png)], Some(&path))).is_err());

        let target = Arc::make_mut(doc.assets.get_mut(&asset).unwrap());
        target.link = Some("/missing/local/source.png".into());
        let retained_link = model_bytes(&doc).unwrap();
        let untrusted = with_members(&encoded, &[("document.json", &retained_link)], Some("document.json"));
        assert_eq!(decode(&untrusted), Err(UNSUPPORTED_LINKS.into()), "only local callers may strip embedded-link provenance");
        Arc::make_mut(doc.assets.get_mut(&asset).unwrap()).storage = Storage::Linked;
        assert_eq!(encode(&doc), Err(UNSUPPORTED_LINKS.into()));
        assert_eq!(fingerprint(&doc), Err(UNSUPPORTED_LINKS.into()));
        let linked_model = model_bytes(&doc).unwrap();
        let linked_bundle = with_members(&encoded, &[("document.json", &linked_model)], Some("document.json"));
        assert_eq!(decode(&linked_bundle), Err(UNSUPPORTED_LINKS.into()));
    }
}
