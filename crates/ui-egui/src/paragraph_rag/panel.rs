//! The panel applies each committed field to the current selection in one undo step.
use super::ParagraphFields;
use crate::DesignApp;
use crate::dialogs::Dialog;
use crate::i18n::tr;
use crate::rtl;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct SelectionAttrs {
    document: u64,
    revision: u64,
    selection: designcraft_doc::Selection,
    // Input buffers are scoped to the selection, so a focus loss after a selection change
    // cannot apply the previous paragraph's unfinished numeric input to the new paragraph.
    scope: u64,
    attrs: Value,
}

fn selection_attrs(app: &mut DesignApp, ui: &egui::Ui) -> Option<Arc<SelectionAttrs>> {
    let state = app.session.active()?;
    let key = ui.make_persistent_id("ragSelection");
    let saved = ui.data(|d| d.get_temp::<Arc<SelectionAttrs>>(key));
    let same_selection = saved.as_ref().is_some_and(|c| c.document == state.uid && c.selection == state.selection);
    if same_selection && saved.as_ref().is_some_and(|c| c.revision == state.revision) {
        return saved;
    }
    let scope = saved.as_ref().map_or(0, |c| if same_selection { c.scope } else { c.scope.wrapping_add(1) });
    let (document, revision, selection) = (state.uid, state.revision, state.selection.clone());
    let mut attrs = app.session.execute("type.selectionAttrs", &json!({"mixed": true})).ok()?;
    if let Some(keys) = attrs["paraMixed"].as_array().cloned() {
        for key in keys.iter().filter_map(Value::as_str) {
            attrs["para"][key] = Value::Null;
        }
    }
    if attrs["ragLineInsets"].is_array() {
        attrs["para"]["ragLineInsets"] = attrs["ragLineInsets"].clone();
    }
    let snapshot = Arc::new(SelectionAttrs { document, revision, selection, scope, attrs });
    ui.data_mut(|d| d.insert_temp(key, snapshot.clone()));
    Some(snapshot)
}

pub(crate) fn panel(app: &mut DesignApp, ui: &mut egui::Ui) {
    let lang = app.ui.language.clone();
    let Some(snapshot) = selection_attrs(app, ui).filter(|s| !s.attrs.is_null()) else {
        rtl::label(ui, tr(&lang, "Select text or a text frame."));
        return;
    };
    let Some(state) = app.session.active() else { return };
    let units = state.doc.settings.horizontal_units;
    let shared = app.session.is_deferring();
    let attrs = &snapshot.attrs;
    let current = &attrs["para"];
    let mut edits = Dialog::new("ragPanel", Value::Null);
    let fields = ParagraphFields { prefix: "", para: current, units, language: attrs["paragraphLanguage"].as_str(), input_scope: snapshot.scope };
    if let Some(language) = fields.show(&lang, ui, &mut edits, shared) {
        let _ = app.run("type.paragraphLanguage", json!({"language": language}));
    }
    if !edits.fields.is_empty() {
        match crate::dialogs::check_justification(&edits, "", current) {
            Ok(()) => {
                let _ = app.run("type.para", json!({"attrs": edits.fields}));
            }
            Err(reason) => app.status(reason),
        }
    }
    ui.separator();
    ui.push_id(("ragPanelStyle", snapshot.scope), |ui| paragraph_style(app, ui, &lang, attrs, shared));
}

fn paragraph_style(app: &mut DesignApp, ui: &mut egui::Ui, lang: &str, attrs: &Value, shared: bool) {
    let Some(state) = app.session.active() else { return };
    let current = attrs["paragraphStyle"].as_str().filter(|_| attrs["paragraphStyleMixed"] != true);
    let shown = match current {
        Some(style) if attrs["paraOverrides"].as_u64().unwrap_or(0) > 0 => format!("{}+", crate::i18n::style_name(lang, style)),
        Some(style) => crate::i18n::style_name(lang, style).to_owned(),
        None => tr(lang, "Mixed").to_owned(),
    };
    let name = tr(lang, "Paragraph Style");
    rtl::label(ui, name);
    let combo = craft_ui::ComboBox::from_id_salt("ragParaStyle")
        .selected_text(rtl::widget(ui, shown))
        .width(ui.available_width().min(210.0))
        .truncate()
        .show_ui(ui, |ui| {
            let mut selected = None;
            for style in &state.doc.styles.paragraph {
                if style.name == designcraft_doc::NO_PARA_STYLE {
                    continue;
                }
                let label = crate::i18n::style_name(lang, &style.name);
                if ui.selectable_label(current == Some(&style.name), rtl::widget(ui, label)).clicked() {
                    selected = Some(style.name.clone());
                }
            }
            selected
        });
    if let Some(name) = crate::widgets::name_combo(combo, name).inner.flatten() {
        let _ = app.run("style.paragraph.apply", json!({"name": name}));
    }
    if ui.add_enabled(!shared, egui::Button::new(rtl::widget(ui, tr(lang, "New Paragraph Style…")))).clicked() {
        crate::dialogs::open_new_rag_style(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use designcraft_geom::Unit;
    use egui::accesskit::Role;
    use egui_kittest::{Harness, kittest::Queryable};

    fn test_panel() -> (Harness<'static, DesignApp>, Value) {
        let mut app = DesignApp::new(designcraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({})).unwrap();
        let frame = app.run("frame.create", json!({"content":"text", "rect":[36,36,230,300], "text":"First\nSecond", "caret":false})).unwrap();
        let story = frame["story"].clone();
        app.run("text.select", json!({"story":story,"anchor":0,"focus":0})).unwrap();
        let mut h = Harness::builder().with_size(egui::vec2(236.0, 1000.0)).build_ui_state(
            |ui, app: &mut DesignApp| {
                let id = egui::Id::unique("ragtime_fonts");
                if ui.data(|data| data.get_temp::<bool>(id)).is_none() {
                    crate::theme::install_fonts(ui.ctx(), "");
                    ui.data_mut(|data| data.insert_temp(id, true));
                    return;
                }
                panel(app, ui);
            },
            app,
        );
        h.run_steps(3);
        (h, story)
    }

    #[test]
    fn dock_edits_follow_the_selection_and_fit_a_narrow_panel() {
        let (mut h, story) = test_panel();
        h.get_by(|n| n.role() == Role::ComboBox && n.label().as_deref() == Some("Rag pattern")).focus();
        h.run_steps(2);
        h.key_press(egui::Key::Space);
        h.run_steps(2);
        h.get_by_label("Alternating edges").click();
        h.run_steps(2);
        h.get_by(|n| n.role() == Role::TextInput && n.label().as_deref() == Some("Short-line inset")).focus();
        h.run_steps(2);
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
        h.get_by(|n| n.role() == Role::TextInput && n.is_focused()).type_text("9 mm");
        h.run_steps(2);
        h.key_press(egui::Key::Tab);
        h.run_steps(2);
        let attrs = h.state_mut().run("type.selectionAttrs", json!({})).unwrap();
        assert_eq!(attrs["para"]["ragPattern"], "alternatingEdges");
        assert!((attrs["para"]["ragInset"].as_f64().unwrap() - Unit::Millimeters.to_pt(9.0)).abs() < 1e-6);
        h.get_by(|n| n.role() == Role::ComboBox && n.label().as_deref() == Some("Paragraph language")).click();
        h.run_steps(2);
        h.get_by_label("French").click();
        h.run_steps(2);
        assert_eq!(h.state_mut().run("type.selectionAttrs", json!({"mixed": true})).unwrap()["paragraphLanguage"], "French");
        h.get_by_label("Hyphenate").click();
        h.run_steps(2);
        assert_ne!(h.state_mut().run("type.selectionAttrs", json!({})).unwrap()["para"]["hyphenate"], attrs["para"]["hyphenate"]);
        h.get_by_label("Fitting limits").click();
        h.run_steps(3);
        let max = h.get_by(|n| n.role() == Role::TextInput && n.label().as_deref() == Some("Glyph Scaling Maximum"));
        assert!(max.rect().right() <= 236.0, "all three fitting fields stay in the panel");
        h.state_mut().run("text.select", json!({"story":story,"anchor":8,"focus":8})).unwrap();
        h.run_steps(2);
        assert_eq!(h.get_by(|n| n.role() == Role::ComboBox && n.label().as_deref() == Some("Rag pattern")).value().as_deref(), Some("Off"));
        assert!(
            h.query_by(|n| n.role() == Role::TextInput && n.label().as_deref() == Some("Glyph Scaling Maximum")).is_some(),
            "the expanded fitting controls stay open when the selection moves"
        );
        h.state_mut().run("text.select", json!({"story":story,"anchor":0,"focus":9})).unwrap();
        h.run_steps(2);
        assert_eq!(h.get_by(|n| n.role() == Role::ComboBox && n.label().as_deref() == Some("Rag pattern")).value().as_deref(), Some("Mixed"));
    }

    #[test]
    fn editing_a_single_four_stop_in_the_dock_preserves_the_other_mixed_insets() {
        let (mut h, story) = test_panel();
        for (pos, insets) in [(0, [4, 12, 8, 20]), (8, [4, 24, 8, 32])] {
            h.state_mut().run("text.select", json!({"story":story, "anchor":pos, "focus":pos})).unwrap();
            h.state_mut().run("type.para", json!({"attrs":{"ragPattern":"fourStop","ragLineInsets":insets}})).unwrap();
        }
        h.state_mut().run("text.select", json!({"story":story, "anchor":0, "focus":11})).unwrap();
        h.run_steps(3);
        let second = h.get_by(|n| n.role() == Role::TextInput && n.label().as_deref() == Some("Line 2"));
        assert!(second.value().unwrap_or_default().is_empty());
        second.focus();
        h.run_steps(2);
        h.get_by(|n| n.role() == Role::TextInput && n.is_focused()).type_text("7 pt");
        h.run_steps(2);
        h.key_press(egui::Key::Tab);
        h.run_steps(2);
        for (pos, last) in [(0, 20.0), (8, 32.0)] {
            h.state_mut().run("text.select", json!({"story":story, "anchor":pos, "focus":pos})).unwrap();
            let attrs = h.state_mut().run("type.selectionAttrs", json!({})).unwrap();
            assert_eq!(attrs["para"]["ragLineInsets"], json!([4.0, 7.0, 8.0, last]));
            h.run_steps(2);
            let sketch = h.get_by(|n| n.role() == Role::Image && n.label().is_some_and(|label| label.starts_with("60 mm preview")));
            assert!(sketch.rect().right() <= 236.0);
        }
    }
}
