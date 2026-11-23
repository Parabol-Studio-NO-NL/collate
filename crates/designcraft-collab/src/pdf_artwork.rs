//! Bounds for PDF files received as encrypted artwork. The native document keeps
//! the original bytes, while this module checks the selected page and crop.

use std::panic::{AssertUnwindSafe, catch_unwind};

use craft_pdf::syntax::{
    DecodeLimits,
    object::{Array, Name, Object, Stream, stream::ImageDecodeParams},
};
use designcraft_doc::{Asset, PdfCrop};
use serde::{Deserialize, Serialize};

use crate::artwork::{MAX_ARTWORK_BYTES, MAX_IMAGE_PIXELS, MAX_IMAGE_SIDE, valid_dimensions};

pub const PDF_MIME: &str = "application/pdf";
const MAX_PDF_PAGES: usize = 128;
const MAX_PDF_OBJECTS: usize = 16_384;
const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;
const MAX_EXPANDED_BYTES: usize = 64 * 1024 * 1024;
const INVALID: &str = "A shared PDF is invalid or exceeds the room limits.";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PdfPlacement {
    pub name: String,
    pub page: u32,
    pub crop: PdfCrop,
    pub bounds: Option<[f64; 4]>,
}

impl PdfPlacement {
    pub fn for_asset(asset: &Asset) -> Self {
        Self { name: asset.name.clone(), page: asset.page, crop: asset.pdf_crop, bounds: asset.pdf_box }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PdfPage {
    pub pixels: (u32, u32),
    pub points: (f64, f64),
}

pub(crate) fn validate_crop(asset: &Asset, allow_pending: bool) -> Result<(), &'static str> {
    match asset.pdf_box {
        None if asset.pdf_crop == PdfCrop::Crop || allow_pending && asset.content_box_pending() => Ok(()),
        Some(_) if asset.pdf_crop != PdfCrop::Crop && asset.shown_box().is_some() => Ok(()),
        _ => Err("A shared PDF has invalid crop metadata."),
    }
}

pub(crate) fn inspect(bytes: &[u8], page: u32, verify_body: bool) -> Result<PdfPage, &'static str> {
    if !bytes.starts_with(b"%PDF-") || bytes.len() > MAX_ARTWORK_BYTES {
        return Err(INVALID);
    }
    // Apply the PDF backend's own limits while it parses xref streams. The
    // aggregate check below bounds the materialized streams in a room asset.
    let options = craft_pdf::OpenOptions {
        max_input_bytes: MAX_ARTWORK_BYTES,
        decode_limits: DecodeLimits::default()
            .with_max_decoded_stream(MAX_STREAM_BYTES)
            .with_max_decoded_image(MAX_EXPANDED_BYTES as u64)
            .with_max_image_pixels(MAX_IMAGE_PIXELS),
    };
    let pdf = craft_pdf::open_slice_with_options(bytes, None, &options).map_err(|_| INVALID)?;
    if pdf.is_encrypted() || pdf.rebuilt_unlinked_update() || pdf.len() > MAX_PDF_OBJECTS || pdf.pages().len() > MAX_PDF_PAGES {
        return Err(INVALID);
    }
    let (w, h) = pdf.pages().get(page as usize).ok_or(INVALID)?.render_dimensions();
    let max_side = MAX_IMAGE_SIDE as f32;
    if !(w > 0.0 && w <= max_side && h > 0.0 && h <= max_side) {
        return Err(INVALID);
    }
    let pixels = (w.round().max(1.0) as u32, h.round().max(1.0) as u32);
    if !valid_dimensions(pixels, MAX_IMAGE_PIXELS) {
        return Err(INVALID);
    }
    if verify_body {
        catch_unwind(AssertUnwindSafe(|| validate_streams(&pdf))).map_err(|_| INVALID)??;
    }
    Ok(PdfPage { pixels, points: (f64::from(w), f64::from(h)) })
}

fn validate_streams(pdf: &craft_pdf::syntax::Pdf) -> Result<(), &'static str> {
    let mut remaining = MAX_EXPANDED_BYTES;
    for object in pdf.objects() {
        let Object::Stream(stream) = object else { continue };
        let is_image = stream.dict().get::<Name<'_>>(b"Subtype").is_some_and(|name| name.as_ref() == b"Image");
        let limit = if is_image { remaining } else { remaining.min(MAX_STREAM_BYTES) };
        // A codec can stop at its limit. Refuse an exact hit instead of
        // accepting a partially decoded asset as complete.
        let size = if is_image { image_size(&stream, limit)? } else { stream.decoded_within(limit).map_err(|_| INVALID)?.len() };
        if size >= limit {
            return Err(INVALID);
        }
        remaining -= size;
    }
    Ok(())
}

fn image_size(stream: &Stream<'_>, limit: usize) -> Result<usize, &'static str> {
    let dict = stream.dict();
    let width: u32 = dict.get(b"Width").ok_or(INVALID)?;
    let height: u32 = dict.get(b"Height").ok_or(INVALID)?;
    let pixel_limit = MAX_IMAGE_PIXELS.min((limit / 4) as u64);
    if !valid_dimensions((width, height), pixel_limit) {
        return Err(INVALID);
    }
    let color = dict.get::<Name<'_>>(b"ColorSpace");
    let color_family = dict.get::<Array<'_>>(b"ColorSpace").and_then(|array| array.iter::<Name<'_>>().next());
    let is_indexed = color_family.as_ref().is_some_and(|name| name.as_ref() == b"Indexed");
    let num_components = if is_indexed {
        Some(1)
    } else {
        color.as_ref().and_then(|name| match name.as_ref() {
            b"DeviceGray" => Some(1),
            b"DeviceRGB" => Some(3),
            b"DeviceCMYK" => Some(4),
            _ => None,
        })
    };
    let params = ImageDecodeParams { width, height, bpc: dict.get(b"BitsPerComponent"), is_indexed, num_components, ..Default::default() };
    let decoded = stream.decoded_image(&params).map_err(|_| INVALID)?;
    let alpha_len = decoded.image_data.and_then(|image| image.alpha).map_or(0, |alpha| alpha.len());
    decoded.data.len().checked_add(alpha_len).ok_or(INVALID)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use craft_image_codecs::{ChannelLayout, Format, Image};

    use super::*;

    #[test]
    fn accepts_embedded_jpeg_and_checks_the_selected_page() {
        let image = Image::from_u8(8, 4, ChannelLayout::Rgb, [45, 126, 200].repeat(32)).unwrap();
        let jpeg = craft_image_codecs::encode(&image, Format::Jpeg, &Default::default()).unwrap();
        let pdf = pdf_with_jpeg(&jpeg, 8);
        let page = inspect(&pdf, 0, true).unwrap();
        assert_eq!(page.pixels, (72, 36));
        assert_eq!(page.points, (72.0, 36.0));
        assert!(inspect(&pdf, 1, true).is_err());
        assert!(inspect(&pdf_with_jpeg(&jpeg, MAX_IMAGE_SIDE + 1), 0, true).is_err());
    }

    #[test]
    fn checks_pdf_crop_metadata() {
        let bounds = Some([0.0, 0.0, 1.0, 0.5]);
        let asset = |pdf_crop, pdf_box| Asset { mime: PDF_MIME.into(), pdf_crop, pdf_box, ..Asset::default() };
        assert!(validate_crop(&asset(PdfCrop::Trim, bounds), false).is_ok());
        assert!(validate_crop(&asset(PdfCrop::Crop, None), false).is_ok());
        assert!(validate_crop(&asset(PdfCrop::Crop, bounds), false).is_err());
        assert!(validate_crop(&asset(PdfCrop::Trim, None), true).is_err());
        assert!(validate_crop(&asset(PdfCrop::ContentVisible, None), true).is_ok());
        assert!(validate_crop(&asset(PdfCrop::ContentVisible, None), false).is_err());
        assert!(validate_crop(&asset(PdfCrop::Media, Some([0.0, 0.0, f64::NAN, 1.0])), false).is_err());
    }

    fn pdf_with_jpeg(jpeg: &[u8], width: u32) -> Vec<u8> {
        const DRAW: &str = "q 72 0 0 36 0 0 cm /Picture Do Q";
        let content = format!("<< /Length {} >>\nstream\n{DRAW}\nendstream", DRAW.len());
        let image_header = format!(
            "<< /Type /XObject /Subtype /Image /Width {width} /Height 4 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
            jpeg.len()
        );
        let image = [image_header.as_bytes(), jpeg, b"\nendstream"].concat();
        let objects: [&[u8]; 5] = [
            b"<< /Type /Catalog /Pages 2 0 R >>",
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 72 36] /Resources << /XObject << /Picture 5 0 R >> >> /Contents 4 0 R >>",
            content.as_bytes(),
            &image,
        ];
        let mut pdf = b"%PDF-1.4\n%\xDB\xDC\xDD\xDE\n".to_vec();
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            writeln!(pdf, "{} 0 obj", i + 1).unwrap();
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        write!(pdf, "xref\n0 6\n0000000000 65535 f \n").unwrap();
        for offset in offsets {
            writeln!(pdf, "{offset:010} 00000 n ").unwrap();
        }
        write!(pdf, "trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").unwrap();
        pdf
    }
}
