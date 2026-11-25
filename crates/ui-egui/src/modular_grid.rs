//! A page-scoped draft for the modular grid calculator. Only Apply changes the document.
use crate::DesignApp;
use crate::i18n::tr;
use crate::rtl;
use crate::theme::Tokens;
use designcraft_doc::{Page, SpreadRef};
use designcraft_engine::modular_grid::{GridFit, GridRequest};
use designcraft_geom::{Unit, format_measure, parse_measure};
use egui::{Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use serde_json::json;

const APPLY: &str = "layout.modularBaselineGrid";

#[derive(Clone)]
struct Draft {
    source: GridRequest,
    columns: String,
    rows: String,
    column_gutter: String,
    gutter_lines: String,
    leading: String,
    fit: bool,
}

impl Draft {
    fn new(source: GridRequest, unit: Unit) -> Self {
        Self {
            columns: source.columns.to_string(),
            rows: source.rows.to_string(),
            column_gutter: format_measure(source.column_gutter, unit),
            gutter_lines: source.gutter_lines.to_string(),
            leading: format_measure(source.leading, Unit::Points),
            fit: source.fit,
            source,
        }
    }

    fn request(&self, unit: Unit) -> Result<GridRequest, &'static str> {
        // Keep the original precision when an existing gutter or leading is displayed rounded.
        let measure = |text: &str, original, unit| {
            if text.trim() == format_measure(original, unit) { Some(original) } else { parse_measure(text, unit).ok() }
        };
        Ok(GridRequest {
            columns: self.columns.trim().parse().map_err(|_| "Enter a whole number of columns.")?,
            rows: self.rows.trim().parse().map_err(|_| "Enter a whole number of rows.")?,
            column_gutter: measure(&self.column_gutter, self.source.column_gutter, unit).ok_or("Enter a distance for the column gutter.")?,
            gutter_lines: self.gutter_lines.trim().parse().map_err(|_| "Enter a whole number of baseline intervals for the row gap.")?,
            leading: measure(&self.leading, self.source.leading, Unit::Points).ok_or("Enter a distance for the target leading.")?,
            fit: self.fit,
        })
    }

    fn fields(&mut self, ui: &mut Ui, lang: &str) {
        ui.columns(2, |cols| {
            field(&mut cols[0], tr(lang, "Columns"), &mut self.columns);
            field(&mut cols[1], tr(lang, "Rows"), &mut self.rows);
        });
        ui.columns(2, |cols| {
            field(&mut cols[0], tr(lang, "Column gutter"), &mut self.column_gutter);
            field(&mut cols[1], tr(lang, "Row gap (lines)"), &mut self.gutter_lines);
        });
        field(ui, tr(lang, "Target leading"), &mut self.leading);
        ui.checkbox(&mut self.fit, rtl::widget(ui, tr(lang, "Fit leading to margins")));
    }
}

fn field(ui: &mut Ui, name: &str, value: &mut String) {
    let label = rtl::label(ui, name);
    ui.add(egui::TextEdit::singleline(value).id_salt(name).desired_width(ui.available_width().min(112.0)).char_limit(64)).labelled_by(label.id);
}

pub(crate) fn panel(app: &mut DesignApp, ui: &mut Ui) {
    let lang = &app.ui.language;
    let Some(state) = app.session.active() else {
        rtl::label(ui, tr(lang, "Open a document to calculate its grid."));
        return;
    };
    let Some((spread, index)) = crate::canvas::page_target(app) else {
        rtl::label(ui, tr(lang, "Select a page to calculate its grid."));
        return;
    };
    let Some(sheet) = state.doc.spread(spread) else { return };
    let Some(page) = sheet.pages.get(index) else { return };
    let settings = &state.doc.settings;
    let unit = settings.horizontal_units;
    let inherited = state.doc.parent_page(page).filter(|p| matches!(spread, SpreadRef::Doc(_)) && !page.local_margins && page.same_margins(p));
    let source = page.modular_grid.or_else(|| inherited.and_then(|p| p.modular_grid)).unwrap_or(GridRequest {
        columns: if page.columns.count == 1 { 3 } else { page.columns.count },
        rows: 4,
        column_gutter: page.columns.gutter,
        gutter_lines: 1,
        leading: settings.baseline_grid.increment,
        fit: true,
    });
    let key = ui.make_persistent_id(("modularGridDraft", state.uid, page.id, unit));
    let mut draft = ui.data(|d| d.get_temp::<Draft>(key)).filter(|d| d.source == source).unwrap_or_else(|| Draft::new(source, unit));
    let target = match spread {
        SpreadRef::Doc(i) => tr(lang, "Page {page}").replace("{page}", &app.session.page_label(state.doc.first_page_of_spread(i) + index)),
        SpreadRef::Parent(_) => tr(lang, "{parent}, page {page}")
            .replace("{parent}", &sheet.parent.as_ref().map_or_else(|| tr(lang, "Parent").to_owned(), |p| p.label()))
            .replace("{page}", &(index + 1).to_string()),
    };
    rtl::label(ui, egui::RichText::new(&target).strong());
    let area = page.margin_rect();
    rtl::label(
        ui,
        tr(lang, "Inside margins {width} × {height}")
            .replace("{width}", &format_measure(area.width(), unit))
            .replace("{height}", &format_measure(area.height(), settings.vertical_units)),
    );
    ui.push_id(key, |ui| draft.fields(ui, lang));
    let result = draft.request(unit).and_then(|request| request.calculate(page).map(|grid| (request, grid)));
    let shared = app.session.is_deferring();
    let mut apply = None;
    ui.separator();
    match result {
        Ok((request, grid)) => {
            let units = (unit, settings.vertical_units);
            draw_preview(ui, lang, page, &request, &grid);
            let module = tr(lang, "Module {width} × {height}")
                .replace("{width}", &format_measure(grid.module_width, units.0))
                .replace("{height}", &format_measure(grid.module_height, units.1));
            rtl::label(ui, egui::RichText::new(module).strong());
            let leading = tr(lang, "{leading} leading, {count} intervals per row")
                .replace("{leading}", &format_measure(grid.effective_leading, Unit::Points))
                .replace("{count}", &grid.lines_per_row.to_string());
            live_label(ui, &leading);
            if grid.remainder > 1e-6 {
                rtl::label(ui, tr(lang, "{space} below last row").replace("{space}", &format_measure(grid.remainder, units.1)));
            }
            apply = Some(request);
        }
        Err(error) => live_label(ui, tr(lang, error)),
    }
    if shared {
        rtl::label(ui, tr(lang, "You can preview a grid in a shared document. Apply it in a local document."));
    } else {
        let scope = match spread {
            SpreadRef::Doc(_) => "Guides apply to this page. All pages use the calculated leading, starting at the top margin.",
            SpreadRef::Parent(_) => "Pages using this parent inherit its guides. All pages use the calculated leading, starting at the top margin.",
        };
        rtl::label(ui, tr(lang, scope));
    }
    ui.data_mut(|d| d.insert_temp(key, draft));
    if ui.add_enabled(apply.is_some() && !shared, egui::Button::new(rtl::widget(ui, tr(lang, "Apply grid")))).clicked()
        && let Some(r) = apply
    {
        let notice = apply_grid(app, r, spread, index, &target);
        app.status(&notice);
        if let Some(state) = app.session.active() {
            ui.data_mut(|d| d.insert_temp(key.with("notice"), (r, state.revision, notice)));
        }
    }
    if let Some((request, revision, notice)) = ui.data(|d| d.get_temp::<(GridRequest, u64, String)>(key.with("notice")))
        && Some(request) == apply
        && app.session.active().is_some_and(|s| s.revision == revision)
    {
        live_label(ui, &notice);
    }
}

fn apply_grid(app: &mut DesignApp, request: GridRequest, spread: SpreadRef, page: usize, target: &str) -> String {
    let mut params = json!(request);
    params["spread"] = json!(spread);
    params["page"] = json!(page);
    if let Err(error) = app.run(APPLY, params) {
        return error;
    }
    if !app.ui.guides {
        let _ = app.run("view.guides", json!({}));
    }
    if !app.ui.baseline_grid {
        let _ = app.run("view.baselineGrid", json!({}));
    }
    tr(&app.ui.language, "Grid applied to {target}.").replace("{target}", target)
}

fn live_label(ui: &mut Ui, message: &str) {
    let response = rtl::label(ui, message);
    ui.ctx().accesskit_node_builder(response.id, |node| node.set_live(egui::accesskit::Live::Polite));
}

fn draw_preview(ui: &mut Ui, lang: &str, page: &Page, request: &GridRequest, grid: &GridFit) {
    let tokens = Tokens::get(ui.ctx());
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, 132.0), Sense::hover());
    let bounds = rect.shrink(5.0);
    if !bounds.is_positive() {
        return;
    }
    let scale = (f64::from(bounds.width()) / page.width).min(f64::from(bounds.height()) / page.height);
    let paper = Rect::from_center_size(bounds.center(), vec2((page.width * scale) as f32, (page.height * scale) as f32));
    let x = |pt: f64| paper.left() + (pt * scale) as f32;
    let y = |pt: f64| paper.top() + (pt * scale) as f32;
    let (left, right) = page.left_right_margins();
    let top = page.margins.top;
    let bottom = page.height - page.margins.bottom - grid.remainder;
    let body = Rect::from_min_max(pos2(x(left), y(top)), pos2(x(page.width - right), y(bottom)));
    let p = ui.painter();
    p.rect_filled(paper, 0.0, tokens.input);
    p.rect_stroke(paper, 0.0, Stroke::new(1.0, tokens.icon), StrokeKind::Inside);
    p.rect_filled(body, 0.0, tokens.row_selected);
    let line = Stroke::new(1.0, tokens.icon);
    // When the page is too small to distinguish individual baselines, sample at visible intervals.
    let lines = ((bottom - top) / grid.effective_leading).round() as usize;
    let stride = (3.0 / (grid.effective_leading * scale)).ceil().max(1.0) as usize;
    for i in (0..=lines).step_by(stride) {
        p.hline(body.x_range(), y(top + i as f64 * grid.effective_leading), Stroke::new(0.5, tokens.ruler_tick));
    }
    for col in 1..request.columns {
        let edge = left + f64::from(col) * (grid.module_width + request.column_gutter);
        let gap = Rect::from_x_y_ranges(x(edge - request.column_gutter)..=x(edge), body.y_range());
        p.rect_filled(gap, 0.0, tokens.input);
        p.vline(gap.left(), body.y_range(), line);
        p.vline(gap.right(), body.y_range(), line);
    }
    for row in 1..request.rows {
        let edge = top + f64::from(row) * (grid.module_height + grid.row_gutter);
        let gap = Rect::from_x_y_ranges(body.x_range(), y(edge - grid.row_gutter)..=y(edge));
        p.rect_filled(gap, 0.0, tokens.input);
        p.hline(body.x_range(), gap.top(), line);
        p.hline(body.x_range(), gap.bottom(), line);
    }
    p.rect_stroke(body, 0.0, line, StrokeKind::Inside);
    let description = tr(lang, "Grid preview, {columns} columns by {rows} rows")
        .replace("{columns}", &request.columns.to_string())
        .replace("{rows}", &request.rows.to_string());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, &description));
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::accesskit::Role;
    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };

    #[test]
    fn a_keyboard_edit_updates_the_preview_before_applying_to_the_page() {
        let mut app = DesignApp::new(designcraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({})).unwrap();
        app.session.active_mut().unwrap().selection.page = Some(0);
        app.ui.guides = false;
        app.ui.baseline_grid = false;
        let mut h = Harness::builder().with_size(vec2(220.0, 720.0)).build_ui_state(|ui, app| panel(app, ui), app);
        h.run_steps(2);
        let field = h.get_by(|n| n.role() == Role::TextInput && n.label().as_deref() == Some("Rows"));
        field.focus();
        h.run_steps(2);
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
        h.get_by(|n| n.role() == Role::TextInput && n.is_focused()).type_text("x");
        h.run_steps(2);
        assert!(h.get_by_label("Apply grid").accesskit_node().is_disabled());
        h.get_by_label("Enter a whole number of rows.");
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
        h.get_by(|n| n.role() == Role::TextInput && n.is_focused()).type_text("5");
        h.run_steps(2);
        let sketch = h.get_by(|n| n.role() == Role::Image && n.label().is_some_and(|v| v.ends_with("by 5 rows")));
        assert!(sketch.rect().right() <= 220.0);
        assert!(h.state().session.active().unwrap().doc.page(0).unwrap().modular_grid.is_none());
        h.get_by_label("Apply grid").focus();
        h.run_steps(2);
        h.key_press(egui::Key::Space);
        h.run_steps(2);
        assert_eq!(h.state().session.active().unwrap().doc.page(0).unwrap().modular_grid.unwrap().rows, 5);
        assert!(h.state().ui.guides && h.state().ui.baseline_grid);
        h.get_by_label("Grid applied to Page 1.");
    }
}
