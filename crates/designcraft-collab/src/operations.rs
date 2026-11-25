use std::mem::discriminant;

use designcraft_doc::color::BlendMode;
use designcraft_doc::geom::corners::CornerOptions;
use designcraft_doc::geom::{Affine, PathData};
use designcraft_doc::{CharRun, Fill, Item, ItemId, LayerId, PageId, ParaFormat, Shape, SpreadId, StoryId, Stroke};
use serde::{Deserialize, Serialize};

use crate::{
    artwork::Placement,
    automerge_text::{AutomergeCommentAnchor, ReversibleCommentAnchor},
    layers::LayerEdit,
    pages::PageEdit,
    text::{AtomId, TextEdit},
};

/// Seed IDs are defined by the immutable starting document. Subsequent IDs are
/// tied to an authenticated actor and a proposal sequence, independent of a Mac's
/// local numeric allocator. The index distinguishes creations in one proposal.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value", deny_unknown_fields)]
pub enum SharedId {
    Seed(ItemId),
    Created { actor: String, sequence: u64, index: u16 },
}

impl SharedId {
    pub fn author(&self) -> Option<&str> {
        match self {
            Self::Seed(_) => None,
            Self::Created { actor, .. } => Some(actor),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    pub shape: Shape,
    pub path: PathData,
    pub transform: [f64; 6],
    pub corners: CornerOptions,
}

impl Geometry {
    fn from_item(item: &Item) -> Self {
        Self { shape: item.shape, path: item.path.clone(), transform: item.xf.as_coeffs(), corners: item.corners }
    }

    pub(crate) fn apply(&self, item: &mut Item) {
        item.shape = self.shape;
        item.path = self.path.clone();
        item.xf = Affine::new(self.transform);
        item.corners = self.corners;
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Appearance {
    pub name: String,
    pub object_style: String,
    pub opacity: f32,
    pub blend: BlendMode,
    pub hidden: bool,
    pub locked: bool,
    pub nonprinting: bool,
}

impl Appearance {
    fn from_item(item: &Item) -> Self {
        Self {
            name: item.name.clone(),
            object_style: item.object_style.clone(),
            opacity: item.opacity,
            blend: item.blend,
            hidden: item.hidden,
            locked: item.locked,
            nonprinting: item.nonprinting,
        }
    }

    pub(crate) fn apply(&self, item: &mut Item) {
        item.name.clone_from(&self.name);
        item.object_style.clone_from(&self.object_style);
        item.opacity = self.opacity;
        item.blend = self.blend;
        item.hidden = self.hidden;
        item.locked = self.locked;
        item.nonprinting = self.nonprinting;
    }
}

/// Only inert frame data crosses the wire. File paths, contents, scripts, and
/// engine command names are absent from this type.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeFrame {
    pub geometry: Geometry,
    pub fill: Fill,
    pub stroke: Stroke,
    pub appearance: Appearance,
    pub layer: LayerId,
}

impl ShapeFrame {
    pub(crate) fn from_item(item: &Item) -> Self {
        Self {
            geometry: Geometry::from_item(item),
            fill: item.fill.clone(),
            stroke: item.stroke.clone(),
            appearance: Appearance::from_item(item),
            layer: item.layer,
        }
    }

    pub(crate) fn to_item(&self, id: ItemId) -> Item {
        let mut item = Item::new(id, self.layer, self.geometry.shape, self.geometry.path.clone());
        item.xf = Affine::new(self.geometry.transform);
        item.corners = self.geometry.corners;
        item.fill.clone_from(&self.fill);
        item.stroke.clone_from(&self.stroke);
        self.appearance.apply(&mut item);
        item
    }

    pub(crate) fn changes(self, before: &Self) -> impl Iterator<Item = Change> {
        [
            (before.geometry != self.geometry).then_some(Change::Geometry(self.geometry)),
            (before.fill != self.fill).then_some(Change::Fill(self.fill)),
            (before.stroke != self.stroke).then_some(Change::Stroke(self.stroke)),
            (before.appearance != self.appearance).then_some(Change::Appearance(self.appearance)),
            (before.layer != self.layer).then_some(Change::Layer(self.layer)),
        ]
        .into_iter()
        .flatten()
    }
}

/// Independent domains for stale-proposal conflict checks.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value", deny_unknown_fields)]
pub enum Change {
    Geometry(Geometry),
    Fill(Fill),
    Stroke(Stroke),
    Appearance(Appearance),
    Layer(LayerId),
}

impl Change {
    pub(crate) fn previous(&self, item: &Item) -> Self {
        match self {
            Self::Geometry(_) => Self::Geometry(Geometry::from_item(item)),
            Self::Fill(_) => Self::Fill(item.fill.clone()),
            Self::Stroke(_) => Self::Stroke(item.stroke.clone()),
            Self::Appearance(_) => Self::Appearance(Appearance::from_item(item)),
            Self::Layer(_) => Self::Layer(item.layer),
        }
    }

    pub(crate) fn apply(&self, item: &mut Item) {
        match self {
            Self::Geometry(value) => value.apply(item),
            Self::Fill(value) => item.fill.clone_from(value),
            Self::Stroke(value) => item.stroke.clone_from(value),
            Self::Appearance(value) => value.apply(item),
            Self::Layer(value) => item.layer = *value,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value", deny_unknown_fields)]
pub enum CommentAnchor {
    Page(PageId),
    Object(SharedId),
    Text { story: StoryId, atoms: Vec<AtomId> },
    AutomergeText { story: StoryId, anchor: AutomergeCommentAnchor },
    ReversibleText { story: StoryId, anchor: ReversibleCommentAnchor },
}

impl CommentAnchor {
    /// Whether navigation must resolve the anchor through a story replica.
    pub fn is_text(&self) -> bool {
        matches!(self, Self::Text { .. } | Self::AutomergeText { .. } | Self::ReversibleText { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentReply {
    pub id: SharedId,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentThread {
    pub id: SharedId,
    pub anchor: CommentAnchor,
    pub text: String,
    pub replies: Vec<CommentReply>,
    pub resolved: bool,
}

/// Formatting of a new standalone text frame, validated by the same rules as story edits.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitialTextFormat {
    pub(crate) chars: Vec<CharRun>,
    pub(crate) paras: Vec<ParaFormat>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
pub enum Operation {
    Create {
        id: SharedId,
        spread: SpreadId,
        frame: Box<ShapeFrame>,
    },
    CreateGraphic {
        id: SharedId,
        spread: SpreadId,
        frame: Box<ShapeFrame>,
        placement: Box<Placement>,
    },
    CreateText {
        id: SharedId,
        spread: SpreadId,
        frame: Box<ShapeFrame>,
        text: String,
        vertical: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<Box<InitialTextFormat>>,
    },
    ConvertText {
        id: SharedId,
    },
    Delete {
        id: SharedId,
        spread: SpreadId,
    },
    DeleteText {
        id: SharedId,
        spread: SpreadId,
        story: StoryId,
    },
    Patch {
        id: SharedId,
        change: Change,
    },
    SetGraphic {
        id: SharedId,
        placement: Option<Box<Placement>>,
    },
    Reorder {
        spread: SpreadId,
        items: Vec<SharedId>,
    },
    Text {
        story: StoryId,
        actor: String,
        sequence: u64,
        edits: Vec<TextEdit>,
    },
    SetTextBatchActive {
        story: StoryId,
        actor: String,
        original_sequence: u64,
        active: bool,
    },
    SetAutomergeTextBatchActive {
        story: StoryId,
        actor: String,
        original_sequence: u64,
        active: bool,
    },
    Page {
        edit: PageEdit,
    },
    Layer {
        edit: LayerEdit,
    },
    CreateComment {
        id: SharedId,
        anchor: CommentAnchor,
        text: String,
    },
    Reply {
        thread: SharedId,
        id: SharedId,
        text: String,
    },
    SetResolved {
        thread: SharedId,
        resolved: bool,
    },
}

impl Operation {
    pub fn new_text_frame(&self) -> Option<&SharedId> {
        match self {
            Self::CreateText { id, .. } | Self::ConvertText { id } => Some(id),
            _ => None,
        }
    }

    pub fn placement(&self) -> Option<&Placement> {
        match self {
            Self::CreateGraphic { placement, .. } | Self::SetGraphic { placement: Some(placement), .. } => Some(placement),
            _ => None,
        }
    }

    pub fn placement_mut(&mut self) -> Option<&mut Placement> {
        match self {
            Self::CreateGraphic { placement, .. } | Self::SetGraphic { placement: Some(placement), .. } => Some(placement),
            _ => None,
        }
    }

    pub fn changes_artwork(&self) -> bool {
        matches!(self, Self::CreateGraphic { .. } | Self::SetGraphic { .. })
    }

    pub fn is_text_compensation(&self) -> bool {
        matches!(self, Self::SetTextBatchActive { .. } | Self::SetAutomergeTextBatchActive { .. })
    }

    /// The host calls this with the authenticated endpoint identity before applying a proposal.
    pub fn is_authored_by(&self, actor: &str) -> bool {
        match self {
            Self::Text { actor: claimed, .. }
            | Self::SetTextBatchActive { actor: claimed, .. }
            | Self::SetAutomergeTextBatchActive { actor: claimed, .. } => claimed == actor,
            _ => self.new_id().is_none_or(|id| id.author() == Some(actor)),
        }
    }

    /// The host can additionally bind object and comment IDs to their proposal sequence.
    pub fn is_from_sequence(&self, sequence: u64) -> bool {
        match self {
            Self::Text { sequence: claimed, .. } => *claimed == sequence,
            _ => self.new_id().is_none_or(|id| matches!(id, SharedId::Created { sequence: value, .. } if *value == sequence)),
        }
    }

    fn new_id(&self) -> Option<&SharedId> {
        match self {
            Self::Create { id, .. }
            | Self::CreateGraphic { id, .. }
            | Self::CreateText { id, .. }
            | Self::CreateComment { id, .. }
            | Self::Reply { id, .. } => Some(id),
            _ => None,
        }
    }

    fn item_changed(&self) -> Option<&SharedId> {
        match self {
            Self::Create { id, .. }
            | Self::CreateGraphic { id, .. }
            | Self::CreateText { id, .. }
            | Self::ConvertText { id }
            | Self::Delete { id, .. }
            | Self::DeleteText { id, .. }
            | Self::Patch { id, .. }
            | Self::SetGraphic { id, .. } => Some(id),
            _ => None,
        }
    }

    fn order_changed(&self) -> Option<SpreadId> {
        match self {
            Self::Create { spread, .. }
            | Self::CreateGraphic { spread, .. }
            | Self::CreateText { spread, .. }
            | Self::Delete { spread, .. }
            | Self::DeleteText { spread, .. }
            | Self::Reorder { spread, .. } => Some(*spread),
            _ => None,
        }
    }

    /// Changes to different properties of one object commute. A deletion
    /// conflicts with edits to its frame and, for text, to its story. Competing stack edits are
    /// checked against their spread, and replies have independent IDs.
    pub fn conflicts(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Page { .. }, _) | (_, Self::Page { .. }) => true,
            (Self::Layer { edit: a }, Self::Layer { edit: b }) => a.conflicts(b),
            (Self::Layer { edit }, other) | (other, Self::Layer { edit }) => {
                edit.conflicts_with_content() && !matches!(other, Self::CreateComment { .. } | Self::Reply { .. } | Self::SetResolved { .. })
            }
            (
                Self::DeleteText { story: a, .. },
                Self::Text { story: b, .. } | Self::SetTextBatchActive { story: b, .. } | Self::SetAutomergeTextBatchActive { story: b, .. },
            )
            | (
                Self::Text { story: b, .. } | Self::SetTextBatchActive { story: b, .. } | Self::SetAutomergeTextBatchActive { story: b, .. },
                Self::DeleteText { story: a, .. },
            ) => a == b,
            (Self::Patch { id: a, change: x }, Self::Patch { id: b, change: y }) => a == b && discriminant(x) == discriminant(y),
            (Self::SetGraphic { id: a, .. }, Self::Patch { id: b, change }) | (Self::Patch { id: b, change }, Self::SetGraphic { id: a, .. }) => {
                a == b && matches!(change, Change::Geometry(_))
            }
            (Self::Reorder { spread, .. }, other) | (other, Self::Reorder { spread, .. }) => other.order_changed() == Some(*spread),
            (Self::SetResolved { thread: a, .. }, Self::SetResolved { thread: b, .. }) => a == b,
            _ => {
                self.item_changed().zip(other.item_changed()).is_some_and(|(a, b)| a == b)
                    || self.new_id().zip(other.new_id()).is_some_and(|(a, b)| a == b)
            }
        }
    }
}
