use std::time::Duration;

use designcraft_collab::SharedId;
use designcraft_collab_core::ActorId;
use designcraft_doc::{Document, SpreadId, SpreadRef};
use designcraft_geom::Point;
use designcraft_tools::CanvasLayout;
use egui::{Color32, Rect, Stroke, StrokeKind};
use serde::{Deserialize, Serialize};
use web_time::Instant;

use super::{Message, Mode, encode_for_protocol};
use crate::{DesignApp, canvas::Xf};

const INTERVAL: Duration = Duration::from_millis(85);
const MAX_SELECTION: usize = 32;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Presence {
    cursor: Option<(SpreadId, [f64; 2])>,
    selection: Vec<SharedId>,
}

impl Presence {
    pub(super) fn is_valid(&self) -> bool {
        self.selection.len() <= MAX_SELECTION
            && self.cursor.is_none_or(|(_, coordinates)| coordinates.into_iter().all(|point| point.is_finite() && point.abs() < 1e6))
    }
}

pub(super) fn color(actor: ActorId) -> Color32 {
    const COLORS: [Color32; 5] = [
        Color32::from_rgb(21, 102, 166),
        Color32::from_rgb(140, 67, 143),
        Color32::from_rgb(15, 118, 88),
        Color32::from_rgb(167, 71, 26),
        Color32::from_rgb(159, 51, 82),
    ];
    COLORS[usize::from(actor.0[0]) % COLORS.len()]
}

impl DesignApp {
    pub(crate) fn draw_collaboration_presence(
        &mut self,
        ui: &egui::Ui,
        painter: &egui::Painter,
        xf: &Xf,
        doc: &Document,
        layout: &CanvasLayout,
        pointer: Option<egui::Pos2>,
    ) {
        let Some(room) = self.collaboration.room.as_mut() else { return };
        let Some(st) = self.session.active().filter(|st| st.uid == room.doc_uid) else { return };
        if let Some(local) = &room.local
            && !room.fatal
            && room.last_presence.is_none_or(|at| at.elapsed() >= INTERVAL)
        {
            let cursor = pointer
                .and_then(|p| layout.spread_at(xf.to_canvas(p)))
                .and_then(|(spread, point)| doc.spread(spread).map(|spread| (spread.id, [point.x, point.y])));
            let selection = st.selection.items.iter().filter_map(|&item| room.replica.shared_id(item)).take(MAX_SELECTION).collect();
            let state = Presence { cursor, selection };
            if matches!(room.mode, Mode::Host { .. }) {
                room.relay_presence(local.actor, state);
            } else if matches!(room.mode, Mode::Guest { ready: true, .. })
                && let Ok(bytes) = encode_for_protocol(Message::Presence { actor: local.actor, state }, room.protocol)
            {
                let _ = room.transport.submit(bytes);
            }
            room.last_presence = Some(Instant::now());
        }

        for (actor, state) in &room.presence {
            let Some(peer) = room.peers.get(actor) else { continue };
            let color = color(*actor);
            for id in &state.selection {
                let Some(local_id) = room.replica.local_id(id) else { continue };
                for slot in &layout.slots {
                    if let Some(item) = doc.spread(slot.spread).and_then(|s| s.items.iter().find(|item| item.id == local_id)) {
                        let bounds = xf.rect((slot.xf * item.xf).transform_rect_bbox(item.inner_bounds()));
                        painter.rect_stroke(bounds, 0.0, Stroke::new(2.0, color), StrokeKind::Outside);
                        break;
                    }
                }
            }
            let Some((id, [x, y])) = state.cursor else { continue };
            let Some(index) = doc.spreads.iter().position(|spread| spread.id == id) else { continue };
            let Some(slot) = layout.slot(SpreadRef::Doc(index)) else { continue };
            let p = xf.to_screen(slot.to_canvas(Point::new(x, y)));
            if !painter.clip_rect().contains(p) {
                continue;
            }
            painter.add(egui::Shape::convex_polygon(
                vec![p, p + egui::vec2(2.0, 15.0), p + egui::vec2(5.5, 10.0), p + egui::vec2(11.5, 9.0)],
                color,
                Stroke::new(0.75, Color32::WHITE),
            ));
            let name = painter.layout_no_wrap(peer.name.clone(), egui::FontId::proportional(11.0), Color32::WHITE);
            let label = p + egui::vec2(12.0, 7.0);
            let background = Rect::from_min_size(label, name.size() + egui::vec2(8.0, 4.0));
            painter.rect_filled(background, 2.0, color);
            painter.galley(label + egui::vec2(4.0, 2.0), name, Color32::WHITE);
        }
        if !room.presence.is_empty() {
            ui.request_repaint_after(INTERVAL);
        }
    }
}
