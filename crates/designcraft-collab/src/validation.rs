use designcraft_doc::color::swatch::NONE;
use designcraft_doc::geom::Affine;
use designcraft_doc::geom::stroke_style::MAX_STRIPE_BANDS;
use designcraft_doc::{Content, Document, Fill, Item, LayerId, NO_OBJECT_STYLE, Shape, Stroke, StrokeType};

use crate::{Appearance, Change, Error, Geometry, ShapeFrame, SharedId};

const MAX_NAME_BYTES: usize = 256;
const MAX_COMMENT_BYTES: usize = 2_048;
const MAX_SUBPATHS: usize = 16;
const MAX_ANCHORS: usize = 256;
const MAX_POINT: f64 = 1_000_000.0;
const MAX_SCALE: f64 = 10_000.0;

pub(crate) fn actor(value: &str) -> Result<(), Error> {
    if value.is_empty() || value.len() > 128 || !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)) {
        return Err(Error::Invalid("actor"));
    }
    Ok(())
}

pub(crate) fn new_id(id: &SharedId) -> Result<(), Error> {
    match id {
        SharedId::Created { actor: name, sequence, .. } if *sequence > 0 => actor(name),
        _ => Err(Error::Invalid("identifier")),
    }
}

pub(crate) fn comment(text: &str) -> Result<(), Error> {
    if text.len() > MAX_COMMENT_BYTES {
        return Err(Error::Limit);
    }
    if text.trim().is_empty() || text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        return Err(Error::Invalid("comment"));
    }
    Ok(())
}

pub(crate) fn editable_frame(item: &Item) -> Result<(), Error> {
    if !matches!(item.content, Content::Unassigned | Content::Graphic(_) | Content::Text(_))
        || item.shape == Shape::Group
        || item.overrides.is_some()
        || item.flex.is_some()
        || item.form_field.is_some()
        || item.live_caption.is_some()
        || item.button.is_some()
        || item.media.is_some()
    {
        return Err(Error::UnsupportedObject);
    }
    Ok(())
}

pub(crate) fn created_frame(doc: &Document, frame: &ShapeFrame) -> Result<(), Error> {
    geometry(&frame.geometry)?;
    fill(doc, &frame.fill)?;
    stroke(doc, &frame.stroke)?;
    appearance(doc, &frame.appearance)?;
    layer(doc, frame.layer)
}

pub(crate) fn change(doc: &Document, value: &Change) -> Result<(), Error> {
    match value {
        Change::Geometry(value) => geometry(value),
        Change::Fill(value) => fill(doc, value),
        Change::Stroke(value) => stroke(doc, value),
        Change::Appearance(value) => appearance(doc, value),
        Change::Layer(value) => layer(doc, *value),
    }
}

fn bounded(value: f64, limit: f64) -> bool {
    (-limit..=limit).contains(&value)
}

fn geometry(value: &Geometry) -> Result<(), Error> {
    if matches!(value.shape, Shape::Group)
        || value.path.is_empty()
        || value.path.subpaths.len() > MAX_SUBPATHS
        || value.path.anchor_count() > MAX_ANCHORS
    {
        return Err(Error::Invalid("shape"));
    }
    let [a, b, c, d, x, y] = value.transform;
    if ![a, b, c, d].into_iter().all(|value| bounded(value, MAX_SCALE)) || ![x, y].into_iter().all(|v| bounded(v, MAX_POINT)) {
        return Err(Error::Invalid("transform"));
    }
    let transform = Affine::new(value.transform);
    if transform.determinant().abs() < 1e-12 {
        return Err(Error::Invalid("transform"));
    }
    for subpath in &value.path.subpaths {
        for anchor in &subpath.anchors {
            for point in [anchor.p, anchor.h_in, anchor.h_out] {
                let transformed = transform * point;
                if ![point.x, point.y, transformed.x, transformed.y].into_iter().all(|value| bounded(value, MAX_POINT)) {
                    return Err(Error::Invalid("path coordinates"));
                }
            }
        }
    }
    if value.corners.corners.iter().any(|corner| !(0.0..=MAX_POINT).contains(&corner.size)) {
        return Err(Error::Invalid("corner size"));
    }
    Ok(())
}

fn name(value: &str) -> bool {
    value.len() <= MAX_NAME_BYTES && !value.chars().any(char::is_control)
}

fn appearance(doc: &Document, value: &Appearance) -> Result<(), Error> {
    if !name(&value.name) || !name(&value.object_style) || !(0.0..=1.0).contains(&value.opacity) {
        return Err(Error::Invalid("appearance"));
    }
    if !value.object_style.is_empty() && value.object_style != NO_OBJECT_STYLE && doc.styles.object_style(&value.object_style).is_none() {
        return Err(Error::UnknownResource);
    }
    Ok(())
}

fn layer(doc: &Document, id: LayerId) -> Result<(), Error> {
    if doc.layers.iter().any(|layer| layer.id == id) {
        return Ok(());
    }
    Err(Error::UnknownResource)
}

fn swatch(doc: &Document, value: &str) -> bool {
    name(value) && (value == NONE || doc.swatches.iter().any(|swatch| swatch.name == value))
}

fn fill(doc: &Document, value: &Fill) -> Result<(), Error> {
    if !swatch(doc, &value.swatch) {
        return Err(Error::UnknownResource);
    }
    if !(0.0..=1.0).contains(&value.tint)
        || value.gradient_angle.is_some_and(|angle| !bounded(angle, MAX_POINT))
        || value.gradient_vector.is_some_and(|vector| !vector.into_iter().all(|point| bounded(point, MAX_POINT)))
    {
        return Err(Error::Invalid("paint"));
    }
    Ok(())
}

fn stroke(doc: &Document, value: &Stroke) -> Result<(), Error> {
    fill(doc, &value.as_fill())?;
    if !swatch(doc, &value.gap_swatch) {
        return Err(Error::UnknownResource);
    }
    if !(0.0..=MAX_SCALE).contains(&value.weight) || !(0.0..=MAX_SCALE).contains(&value.miter_limit) || !(0.0..=1.0).contains(&value.gap_tint) {
        return Err(Error::Invalid("stroke"));
    }
    match &value.kind {
        StrokeType::Dashed { pattern } if !StrokeType::valid_dash_pattern(pattern) => Err(Error::Invalid("dash pattern")),
        StrokeType::Stripes { bands }
            if bands.len() > MAX_STRIPE_BANDS
                || bands.is_empty()
                || !bands.iter().all(|&(start, width)| start >= 0.0 && width > 0.0 && start + width <= 1.0) =>
        {
            Err(Error::Invalid("stroke bands"))
        }
        StrokeType::Style { name: value } if !name(value) || !doc.stroke_styles.iter().any(|style| style.name == *value) => {
            Err(Error::UnknownResource)
        }
        _ => Ok(()),
    }
}
