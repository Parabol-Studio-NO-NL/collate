use designcraft_doc::{ItemId, TextSel};
use designcraft_geom::{Unit, format_measure};

use super::{DesignApp, Tokens, Value, divider, json, sel_info};
use crate::rtl::{self, label};
use crate::{i18n::tr, panels::SelInfo};

pub fn info_panel(app: &mut DesignApp, ui: &mut egui::Ui) {
    let tokens = Tokens::get(ui.ctx());
    let lang = app.ui.language.clone();
    if let Some((state, info)) = app.session.active().zip(info_geometry(app)) {
        let settings = &state.doc.settings;
        let units = display_units(ui, &lang);
        let horizontal = units.unwrap_or(settings.horizontal_units);
        let vertical = units.unwrap_or(settings.vertical_units);
        let x = |v| format_measure(v, horizontal);
        let y = |v| format_measure(v, vertical);
        label(ui, format!("X: {}   Y: {}", x(info.page_rect.x0), y(info.page_rect.y0)));
        label(ui, format!("W: {}   H: {}", x(info.page_rect.width()), y(info.page_rect.height())));
        label(ui, format!("{}: {}   {}: {}", tr(&lang, "Fill"), info.fill, tr(&lang, "Stroke"), info.stroke));
    } else if let Some(state) = app.session.active().filter(|state| state.selection.text.is_none()) {
        label(
            ui,
            format!(
                "{} · {} · {}",
                crate::i18n::count_label(&lang, "pages", state.doc.page_count()),
                crate::i18n::count_label(&lang, "stories", state.doc.stories.len()),
                crate::i18n::count_label(&lang, "items", state.doc.all_items().len())
            ),
        );
    }

    if let Some(counts) = text_statistics(app, ui) {
        divider(ui);
        let scope = match counts["scope"].as_str() {
            Some("selection") => "Selection",
            Some("cell") => "Table Cell",
            Some("footnote") => "Footnote",
            _ => "Story",
        };
        label(ui, egui::RichText::new(tr(&lang, scope)).strong());
        for (caption, key) in [("Characters", "characters"), ("Words", "words"), ("Lines", "lines"), ("Paragraphs", "paragraphs")] {
            label(ui, format!("{}: {}", tr(&lang, caption), counts[key]));
        }
        if let Some(codes) = counts["unicode"].as_array().filter(|codes| !codes.is_empty()) {
            let preview = codes.iter().filter_map(Value::as_u64).map(|code| format!("U+{code:04X}")).collect::<Vec<_>>().join(" ");
            let more = if counts["unicodeTruncated"].as_bool().unwrap_or(false) { "…" } else { "" };
            label(ui, format!("{}: {preview}{more}", tr(&lang, "Unicode")));
        }
    }

    label(ui, egui::RichText::new(format!("{} {:.1} ms", tr(&lang, "Render"), app.perf.render_ms)).color(tokens.text_dim));
}

fn info_geometry(app: &DesignApp) -> Option<SelInfo> {
    sel_info(app).or_else(|| {
        let state = app.session.active()?;
        let text = state.selection.text?;
        let composed_frame = if text.cell.is_none() {
            let layout = app.session.cache.get(&state.doc, text.story, None);
            designcraft_compose::caret(&layout, text.focus).and_then(|(frame, ..)| layout.frames.get(frame)).map(|frame| frame.frame)
        } else {
            None
        };
        let frame = composed_frame.or(text.frame).or_else(|| state.doc.story(text.story)?.frames.first().copied())?;
        crate::panels::sel_info_for_items(app, &[frame])
    })
}

/// Measurement units for this palette. The document's rulers keep their own units.
fn display_units(ui: &mut egui::Ui, lang: &str) -> Option<Unit> {
    let key = egui::Id::unique("info_measurement_units");
    let mut unit = ui.data(|d| d.get_temp::<Option<Unit>>(key)).flatten();
    let previous = unit;
    ui.horizontal(|ui| {
        let caption = label(ui, tr(lang, "Units"));
        craft_ui::ComboBox::from_id_salt("info_measurement_units")
            .selected_text(rtl::widget(ui, tr(lang, unit.map_or("Document Units", Unit::label))))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut unit, None, rtl::widget(ui, tr(lang, "Document Units")));
                for value in Unit::ALL {
                    ui.selectable_value(&mut unit, Some(value), rtl::widget(ui, tr(lang, value.label())));
                }
            })
            .response
            .labelled_by(caption.id);
    });
    if unit != previous {
        ui.data_mut(|d| d.insert_temp(key, unit));
    }
    unit
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct StatisticsSource {
    document: (u64, u64),
    text: Option<TextSel>,
    item: Option<ItemId>,
    table_cells_selected: bool,
}

fn text_statistics(app: &mut DesignApp, ui: &egui::Ui) -> Option<Value> {
    let state = app.session.active()?;
    // The selection can change without an edit when leaving the Type tool.
    let source = StatisticsSource {
        document: (state.uid, state.revision),
        text: state.selection.text,
        item: match state.selection.items.as_slice() {
            [item] => Some(*item),
            _ => None,
        },
        table_cells_selected: state.selection.cells.is_some(),
    };
    let key = ui.make_persistent_id("info_text_statistics");
    if let Some((saved, counts)) = ui.data(|d| d.get_temp::<(StatisticsSource, Value)>(key))
        && saved == source
    {
        return (!counts.is_null()).then_some(counts);
    }
    let counts = app.session.execute("type.statistics", &json!({})).ok()?;
    ui.data_mut(|d| d.insert_temp(key, (source, counts.clone())));
    (!counts.is_null()).then_some(counts)
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    #[test]
    fn info_counts_change_with_the_text_selection() {
        let mut app = DesignApp::new(designcraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({})).unwrap();
        let frame = app
            .run(
                "frame.create",
                json!({
                    "rect": [36, 36, 300, 200], "content": "text", "text": "First line\nSecond line", "caret": false
                }),
            )
            .unwrap();
        let mut harness = Harness::builder().with_size(egui::vec2(260.0, 360.0)).build_ui_state(|ui, app: &mut DesignApp| info_panel(app, ui), app);
        harness.run_steps(2);
        harness.get_by_label("Story");
        harness.get_by_label("Words: 4");
        harness.get_by_label("Lines: 2");
        harness.get_by_label("Paragraphs: 2");

        let state = harness.state().session.active().unwrap();
        let settings = &state.doc.settings;
        let measurements = format!("W: {}   H: {}", format_measure(264.0, settings.horizontal_units), format_measure(164.0, settings.vertical_units));
        harness.get_by_label(measurements.as_str());

        harness.state_mut().session.set_tool("type");
        harness.state_mut().run("text.select", json!({"story": frame["story"], "anchor": 0, "focus": 5})).unwrap();
        harness.run_steps(2);
        harness.get_by_label("Selection");
        harness.get_by_label("Characters: 5");
        harness.get_by_label("Words: 1");
        harness.get_by_label("Lines: 1");
        harness.get_by_label("Paragraphs: 1");
        harness.get_by_label("Unicode: U+0046 U+0069 U+0072 U+0073 U+0074");
        harness.get_by_label(measurements.as_str());

        harness.state_mut().session.set_tool("selection");
        harness.run_steps(2);
        harness.get_by_label("Story");
        harness.get_by_label("Words: 4");
    }
}
