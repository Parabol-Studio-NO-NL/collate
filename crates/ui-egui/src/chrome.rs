//! Window chrome: application bar (with menus), Control panel, document tabs, status bar, start screen.

use designcraft_doc::{Align, Content};
use designcraft_geom::Unit;
use egui::{Color32, Sense, Stroke, vec2};
use serde_json::json;

use crate::panels;
use crate::theme::{Tokens, semibold};
use crate::widgets::{NumField, caption, measure};
use crate::{DesignApp, icons};

mod app_bar;
mod control_bar;
mod start_screen;
mod status_bar;

pub(crate) use app_bar::resize_borders;
#[cfg(test)]
use app_bar::search_id;
pub use app_bar::{app_bar, search_had_focus};
pub use control_bar::control_bar;
pub(crate) use control_bar::{icon_field, swatch_field};
pub(crate) use start_screen::recent_file_stat;
pub use start_screen::start_screen;
#[cfg(test)]
use start_screen::{RECENT_STAT_MS, recent_file_stat_at};
#[cfg(test)]
use status_bar::DOC_TAB_MAX_W;
pub use status_bar::{doc_tabs, status_bar};

/// Height of the application bar (InDesign 2026), then a 7 pt lower band and a 1 pt border. The
/// band covers the panel's 1 pt separator line, so the frame's bottom margin is 1 pt short of it.
const APP_BAR: f32 = 36.0;
const APP_BAR_BAND: f32 = 7.0;
const SEPARATOR: f32 = 1.0;
/// Height of the application bar on macOS, where it is the window's title bar (#333). AppKit draws
/// the traffic lights itself, centred in the standard 28 pt title-bar band at the top of the
/// window; moving them would take AppKit calls (`unsafe`, which the workspace forbids). So the bar
/// is that band: the lights sit vertically centred in it, with the bar's controls beside them.
pub const MAC_APP_BAR: f32 = 28.0;
/// Native points reserved until the main window reports its actual traffic-light frames.
/// On recent macOS the green light ends at 69 pt and the gap between lights is 9 pt.
pub const MAC_CONTENT_LEFT: f32 = 78.0;

impl DesignApp {
    fn session_home(&mut self) {
        self.ui.status = crate::i18n::tr(&self.ui.language, "Home").into();
    }
}

#[cfg(test)]
mod tests;
