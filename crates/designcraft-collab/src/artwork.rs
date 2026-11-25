//! Content-addressed embedded raster and PDF placement. Placements carry no filesystem paths.

use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use craft_image_codecs::{DecodeOptions, Format};
use designcraft_doc::geom::Affine;
use designcraft_doc::{Asset, AssetData, AssetId, Document, Graphic, MAX_NEXT_ID, Storage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Error,
    pdf_artwork::{self, PdfPlacement},
};

pub use pdf_artwork::PDF_MIME;

pub const MAX_IMAGE_SIDE: u32 = 12_000;
pub const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
pub use designcraft_doc::MAX_SHARED_ARTWORK_BYTES as MAX_ARTWORK_BYTES;
const MAX_LEGACY_INLINE_BYTES: usize = 2 * 1024 * 1024;

/// Identifies immutable bytes independently of a local filename or asset allocation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlobId {
    pub digest: [u8; 32],
    pub length: u32,
}

impl BlobId {
    pub fn valid(self) -> bool {
        (1..=MAX_ARTWORK_BYTES).contains(&(self.length as usize))
    }
}
const MAX_POINTS: f64 = 1_000_000.0;
const MAX_SCALE: f64 = 10_000.0;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    digest: [u8; 32],
    size: (f64, f64),
    transform: [f64; 6],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pdf: Option<PdfPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artwork: Option<Artwork>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Artwork {
    name: String,
    mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bytes: Option<Arc<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    length: Option<u32>,
    #[serde(skip)]
    body: Option<Arc<Vec<u8>>>,
}

impl Placement {
    pub(crate) fn capture(graphic: &Graphic, before: &Document, after: &Document) -> Result<Self, Error> {
        if graphic.layer_options.is_some()
            || graphic.clip.is_some()
            || graphic.auto_fit != Default::default()
            || graphic.fit_align != 4
            || graphic.crop != [0.0; 4]
            || graphic.wrap != Default::default()
            || graphic.fill != Default::default()
        {
            return Err(Error::UnsupportedObject);
        }
        let asset = after.assets.get(&graphic.asset).ok_or(Error::UnknownResource)?;
        let bytes = embedded_bytes(asset).ok_or(Error::UnsupportedObject)?;
        let pdf = (asset.mime == PDF_MIME).then(|| PdfPlacement::for_asset(asset));
        // The same PDF can back multiple native assets, one per page and crop.
        let known_file = before.assets.contains_key(&graphic.asset)
            || pdf.is_some() && before.assets.values().filter(|asset| asset.mime == PDF_MIME).any(|asset| embedded_bytes(asset) == Some(bytes));
        let artwork = if known_file {
            None
        } else {
            if bytes.len() > MAX_ARTWORK_BYTES {
                return Err(Error::Limit);
            }
            Some(Artwork {
                name: asset.name.clone(),
                mime: asset.mime.clone(),
                bytes: None,
                length: Some(bytes.len() as u32),
                body: asset.data.embedded_bytes().cloned(),
            })
        };
        Ok(Self { digest: Sha256::digest(bytes).into(), size: graphic.size, transform: graphic.xf.as_coeffs(), pdf, artwork })
    }

    /// Bytes retained by this placement in room history, including earlier inline records.
    pub fn artwork_size(&self) -> u64 {
        let Some(artwork) = &self.artwork else { return 0 };
        artwork.length.map(u64::from).unwrap_or_else(|| artwork.bytes.as_deref().map_or(0, |b| b.len() as u64 * 3 / 4))
    }

    pub fn blob(&self) -> Option<(BlobId, Option<&Arc<Vec<u8>>>)> {
        let artwork = self.artwork.as_ref()?;
        Some((BlobId { digest: self.digest, length: artwork.length? }, artwork.body.as_ref()))
    }

    /// The transport or durable store has checked this immutable body's length and digest.
    pub fn attach_verified(&mut self, body: Arc<Vec<u8>>) -> Result<(), Error> {
        let artwork = self.artwork.as_mut().ok_or(Error::UnknownResource)?;
        if artwork.bytes.is_some() || artwork.length.map(|v| v as usize) != Some(body.len()) || body.len() > MAX_ARTWORK_BYTES {
            return Err(Error::Invalid("artwork attachment"));
        }
        artwork.body = Some(body);
        Ok(())
    }

    pub(crate) fn install(&self, document: &mut Document) -> Result<Graphic, Error> {
        let (width, height) = self.size;
        let [a, b, c, d, x, y] = self.transform;
        let xf = Affine::new(self.transform);
        if ![width, height].into_iter().all(|value| value > 0.0 && value <= MAX_POINTS)
            || ![a, b, c, d].into_iter().all(|v| v.abs() <= MAX_SCALE)
            || ![x, y].into_iter().all(|v| v.abs() <= MAX_POINTS)
            || xf.determinant().abs() < 1e-12
        {
            return Err(Error::Invalid("image placement"));
        }
        // File > Place reserves an asset ID even when it reuses an embedded copy.
        let next = document.next_id.checked_add(1).filter(|next| *next <= MAX_NEXT_ID).ok_or(Error::Limit)?;
        let fresh = AssetId(next);
        if document.assets.contains_key(&fresh) {
            return Err(Error::DuplicateId);
        }
        let asset = if let Some(pdf) = &self.pdf {
            self.install_pdf(document, fresh, pdf)?
        } else if let Some(artwork) = &self.artwork {
            document.add_asset(artwork.decode_raster(fresh, self.digest)?)
        } else {
            self.existing_file(document, |asset| raster_format(&asset.mime).is_some())?.id
        };
        document.next_id = next;
        Ok(Graphic {
            layer_options: None,
            asset,
            size: self.size,
            xf,
            clip: None,
            auto_fit: Default::default(),
            fit_align: 4,
            crop: [0.0; 4],
            wrap: Default::default(),
            fill: Default::default(),
        })
    }

    fn existing_file<'a>(&self, document: &'a Document, matches_kind: impl Fn(&Asset) -> bool) -> Result<&'a Asset, Error> {
        document
            .assets
            .values()
            .map(Arc::as_ref)
            .filter(|asset| matches_kind(asset))
            .find(|asset| embedded_bytes(asset).is_some_and(|bytes| <[u8; 32]>::from(Sha256::digest(bytes)) == self.digest))
            .ok_or(Error::UnknownResource)
    }

    fn install_pdf(&self, document: &mut Document, id: AssetId, placement: &PdfPlacement) -> Result<AssetId, Error> {
        check_name(&placement.name)?;
        let data = match &self.artwork {
            Some(artwork) if artwork.mime == PDF_MIME && artwork.name == placement.name => AssetData::embedded(artwork.decode_bytes(self.digest)?),
            Some(_) => return Err(Error::Invalid("PDF artwork")),
            None => self.existing_file(document, |asset| asset.mime == PDF_MIME)?.data.clone(),
        };
        let page = pdf_artwork::inspect(data.embedded_bytes().ok_or(Error::Invalid("PDF artwork"))?, placement.page, self.artwork.is_some())
            .map_err(|_| Error::Invalid("embedded PDF"))?;
        let asset = Asset {
            id,
            name: placement.name.clone(),
            mime: PDF_MIME.to_owned(),
            data,
            page: placement.page,
            pixels: Some(page.pixels),
            pdf_crop: placement.crop,
            pdf_box: placement.bounds,
            ..Asset::default()
        };
        pdf_artwork::validate_crop(&asset, false).map_err(|_| Error::Invalid("PDF crop"))?;
        let expected = match asset.shown_box() {
            Some([x0, y0, x1, y1]) => ((x1 - x0) * page.points.0, (y1 - y0) * page.points.1),
            None => (page.points.0.max(1e-3), page.points.1.max(1e-3)),
        };
        if (expected.0 - self.size.0).abs() > 1e-6 || (expected.1 - self.size.1).abs() > 1e-6 {
            return Err(Error::Invalid("PDF placement size"));
        }
        Ok(document.add_asset(asset))
    }
}

impl Artwork {
    fn decode_bytes(&self, digest: [u8; 32]) -> Result<Arc<Vec<u8>>, Error> {
        check_name(&self.name)?;
        if let Some(len) = self.length {
            let data = self.body.as_ref().ok_or(Error::UnknownResource)?;
            if self.bytes.is_some() || len as usize != data.len() || data.len() > MAX_ARTWORK_BYTES {
                return Err(Error::Limit);
            }
            return Ok(data.clone());
        }
        let bytes = self.bytes.as_deref().ok_or(Error::UnknownResource)?;
        if bytes.len() > (MAX_LEGACY_INLINE_BYTES * 4).div_ceil(3) {
            return Err(Error::Limit);
        }
        let data = STANDARD_NO_PAD.decode(bytes.as_bytes()).map_err(|_| Error::Invalid("image data"))?;
        if <[u8; 32]>::from(Sha256::digest(&data)) != digest {
            return Err(Error::Invalid("image data"));
        }
        Ok(Arc::new(data))
    }

    fn decode_raster(&self, id: AssetId, digest: [u8; 32]) -> Result<Asset, Error> {
        let data = self.decode_bytes(digest)?;
        raster_dimensions(&self.mime, &data, MAX_IMAGE_PIXELS, true).map_err(|_| Error::Invalid("embedded image"))?;
        let pixels = designcraft_images::pixel_size(&data)
            .filter(|&size| valid_dimensions(size, MAX_IMAGE_PIXELS))
            .ok_or(Error::Invalid("image dimensions"))?;
        Ok(Asset { id, name: self.name.clone(), mime: self.mime.clone(), data: AssetData::embedded(data), pixels: Some(pixels), ..Asset::default() })
    }
}

fn check_name(name: &str) -> Result<(), Error> {
    if name.is_empty() || name.len() > 255 || name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\')) {
        return Err(Error::Invalid("artwork name"));
    }
    Ok(())
}

fn embedded_bytes(asset: &Asset) -> Option<&[u8]> {
    if asset.storage != Storage::Embedded
        || asset.link.is_some()
        || asset.link_relative.is_some()
        || asset.link_hash.is_some()
        || asset.link_fingerprint.is_some()
        || (asset.mime != PDF_MIME && raster_format(&asset.mime).is_none())
    {
        return None;
    }
    asset.data.embedded_bytes().map(|bytes| bytes.as_slice())
}

pub fn pdf_dimensions(asset: &Asset, bytes: &[u8], verify_body: bool) -> Result<(u32, u32), &'static str> {
    pdf_artwork::validate_crop(asset, true)?;
    let page = pdf_artwork::inspect(bytes, asset.page, verify_body)?;
    if asset.pixels.is_some_and(|pixels| pixels != page.pixels) {
        return Err("A shared PDF has invalid page dimensions.");
    }
    Ok(page.pixels)
}

pub fn raster_format(mime: &str) -> Option<(Format, &'static str)> {
    Some(match mime {
        "image/png" => (Format::Png, "png"),
        "image/jpeg" => (Format::Jpeg, "jpg"),
        "image/gif" => (Format::Gif, "gif"),
        "image/webp" => (Format::WebP, "webp"),
        _ => return None,
    })
}

pub fn valid_dimensions((width, height): (u32, u32), limit: u64) -> bool {
    (1..=MAX_IMAGE_SIDE).contains(&width) && (1..=MAX_IMAGE_SIDE).contains(&height) && u64::from(width) * u64::from(height) <= limit
}

pub fn raster_dimensions(mime: &str, bytes: &[u8], max_pixels: u64, verify_body: bool) -> Result<(u32, u32), &'static str> {
    let (format, _) = raster_format(mime).ok_or("Shared documents support embedded PNG, JPEG, GIF and WebP images.")?;
    if craft_image_codecs::detect(bytes) != Some(format) {
        return Err("A shared image does not match its declared format.");
    }
    let dimensions = craft_image_codecs::probe_allocation_size(bytes).map_err(|_| "A shared image is invalid.")?;
    if !valid_dimensions(dimensions, max_pixels) {
        return Err("A shared image exceeds the room pixel limit.");
    }
    if verify_body {
        let limits = craft_image_codecs::Limits::new(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE, max_pixels, max_pixels * 8);
        let options = DecodeOptions { limits, keep_orientation: true };
        craft_image_codecs::decode_as_with(format, bytes, &options).map_err(|_| "A shared image is invalid.")?;
    }
    Ok(dimensions)
}
