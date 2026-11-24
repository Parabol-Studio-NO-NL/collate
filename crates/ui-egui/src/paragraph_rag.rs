//! Collate's rag controls, shared by paragraph formatting and paragraph styles.
use crate::dialogs::Dialog;
use designcraft_geom::Unit;
use serde_json::{Value, json};

pub(crate) const COMMAND: &str = "app.paragraphRagDialog";
pub(crate) const LABEL: &str = "Justification and Rag…";

const RAG_PATTERNS: [(&str, &str); 4] =
    [("none", "Off"), ("natural", "Natural rag"), ("stepped", "Stepped justification"), ("alternatingEdges", "Alternating edges")];

pub(crate) fn validate(d: &Dialog, prefix: &str) -> Result<(), String> {
    if let Some(v) = d.fields.get(&format!("{prefix}ragPattern"))
        && !RAG_PATTERNS.iter().any(|(k, _)| v.as_str() == Some(k))
    {
        return Err("Choose a rag pattern from the list.".into());
    }
    if let Some(v) = d.fields.get(&format!("{prefix}ragInset"))
        && !v.as_f64().is_some_and(|points| (0.0..=designcraft_doc::MAX_MEASURE).contains(&points))
    {
        return Err("Enter a short-line inset between 0 and 15552 pt.".into());
    }
    Ok(())
}

pub(crate) fn show(lang: &str, ui: &mut egui::Ui, d: &mut Dialog, prefix: &str, current: &Value, units: Unit) {
    let value = |d: &Dialog, key: &str| d.fields.get(&format!("{prefix}{key}")).unwrap_or(&current[key]).clone();
    ui.horizontal(|ui| {
        let caption = crate::rtl::label(ui, crate::i18n::tr(lang, "Rag pattern"));
        let pattern = value(d, "ragPattern");
        let shown = RAG_PATTERNS.iter().find(|(key, _)| pattern.as_str() == Some(key)).map_or("", |(_, label)| label);
        craft_ui::ComboBox::from_id_salt(("ragPattern", prefix))
            .selected_text(crate::rtl::widget(ui, crate::i18n::tr(lang, shown)))
            .width(150.0)
            .show_ui(ui, |ui| {
                for (key, label) in RAG_PATTERNS {
                    if ui.selectable_label(pattern.as_str() == Some(key), crate::rtl::widget(ui, crate::i18n::tr(lang, label))).clicked() {
                        let field = format!("{prefix}ragPattern");
                        if current["ragPattern"].as_str() == Some(key) {
                            d.fields.remove(&field);
                        } else {
                            d.fields.insert(field, json!(key));
                        }
                    }
                }
            })
            .response
            .labelled_by(caption.id);
    });
    ui.horizontal(|ui| {
        let name = crate::i18n::tr(lang, "Short-line inset");
        let caption = crate::rtl::label(ui, name);
        let inset = value(d, "ragInset").as_f64();
        let enabled = value(d, "ragPattern").as_str() != Some("none");
        if let Some(points) = crate::widgets::NumField::measure(&format!("ps{prefix}ragInset"), inset, units)
            .width(80.0)
            .name(name)
            .labelled_by(caption.id)
            .enabled(enabled)
            .show(ui)
        {
            d.fields.insert(format!("{prefix}ragInset"), json!(points));
        }
    });
}
