//! Collate's paragraph composition controls, shared by the dock and paragraph styles.
use crate::dialogs::Dialog;
use crate::i18n::tr;
use crate::rtl;
use crate::widgets::NumField;
use designcraft_geom::Unit;
use serde_json::{Value, json};

mod panel;
pub(crate) use panel::panel;

/// Stable entry point for the original modal editor and its automation clients.
pub(crate) const COMMAND: &str = "app.paragraphRagDialog";
pub(crate) const LABEL: &str = "Justification and Rag…";

const RAG_PATTERNS: [(&str, &str); 7] = [
    ("none", "Off"),
    ("natural", "Natural rag"),
    ("stepped", "Stepped justification"),
    ("alternatingEdges", "Alternating edges"),
    ("fourStop", "Four-stop rag"),
    ("nearFlush", "Near-flush"),
    ("contextualIndent", "Contextual indent"),
];

pub(crate) fn validate(d: &Dialog, prefix: &str) -> Result<(), String> {
    if let Some(v) = d.fields.get(&format!("{prefix}ragPattern"))
        && !RAG_PATTERNS.iter().any(|(k, _)| v.as_str() == Some(k))
    {
        return Err("Choose a rag pattern from the list.".into());
    }
    let valid_length = |v: &Value| v.as_f64().is_some_and(|p| (0.0..=designcraft_doc::MAX_MEASURE).contains(&p));
    for key in ["ragInset", "ragNearThreshold"] {
        if let Some(v) = d.fields.get(&format!("{prefix}{key}"))
            && !valid_length(v)
        {
            return Err("Enter a distance between 0 and 15552 pt.".into());
        }
    }
    if let Some(stops) = d.fields.get(&format!("{prefix}ragLineInsets"))
        && !stops.as_array().is_some_and(|a| a.len() == 4 && a.iter().all(|v| v.is_null() || valid_length(v)))
    {
        return Err("Enter four line insets between 0 and 15552 pt.".into());
    }
    Ok(())
}

fn value<'a>(d: &'a Dialog, prefix: &str, current: &'a Value, key: &str) -> &'a Value {
    d.fields.get(&format!("{prefix}{key}")).unwrap_or(&current[key])
}

fn edit(d: &mut Dialog, prefix: &str, current: &Value, key: &str, v: Value) {
    let field = format!("{prefix}{key}");
    if current[key] == v {
        d.fields.remove(&field);
    } else {
        d.fields.insert(field, v);
    }
}

pub(crate) fn show(lang: &str, ui: &mut egui::Ui, d: &mut Dialog, prefix: &str, current: &Value, units: Unit) {
    let name = tr(lang, "Rag pattern");
    rtl::label(ui, name);
    let pattern = value(d, prefix, current, "ragPattern").as_str();
    let shown = RAG_PATTERNS.iter().find(|(key, _)| pattern == Some(key)).map_or("Mixed", |(_, label)| *label);
    let combo = craft_ui::ComboBox::from_id_salt(("ragPattern", prefix))
        .selected_text(rtl::widget(ui, tr(lang, shown)))
        .width(ui.available_width().min(210.0))
        .truncate()
        .show_ui(ui, |ui| {
            let mut picked = None;
            for (key, label) in RAG_PATTERNS {
                if ui.selectable_label(pattern == Some(key), rtl::widget(ui, tr(lang, label))).clicked() && pattern != Some(key) {
                    picked = Some(key);
                }
            }
            picked
        });
    if let Some(key) = crate::widgets::name_combo(combo, name).inner.flatten() {
        edit(d, prefix, current, "ragPattern", json!(key));
    }
    let pattern = value(d, prefix, current, "ragPattern").as_str();
    let (key, caption) = match pattern {
        Some("fourStop") => return four_stops(lang, ui, d, prefix, current, units),
        Some("contextualIndent") => {
            rtl::label(ui, tr(lang, "The first line starts at the preceding paragraph’s last character in the same column."));
            return;
        }
        Some("nearFlush") => ("ragNearThreshold", "Near-flush threshold"),
        _ => ("ragInset", "Short-line inset"),
    };
    let inset = value(d, prefix, current, key).as_f64();
    let field_id = format!("ps{prefix}{key}");
    let field = NumField::measure(&field_id, inset, units).range(0.0, designcraft_doc::MAX_MEASURE).enabled(pattern != Some("none"));
    if let Some(points) = number_row(ui, tr(lang, caption), field) {
        edit(d, prefix, current, key, json!(points));
    }
    if value(d, prefix, current, "ragPattern") == "nearFlush" {
        rtl::label(ui, tr(lang, "Nonfinal lines ending within this distance of the edge may stretch."));
    }
}

fn four_stops(lang: &str, ui: &mut egui::Ui, d: &mut Dialog, prefix: &str, current: &Value, units: Unit) {
    let key = format!("{prefix}ragLineInsets");
    let mut stops: [Option<f64>; 4] =
        std::array::from_fn(|i| d.fields.get(&key).and_then(|v| v[i].as_f64()).or_else(|| current["ragLineInsets"][i].as_f64()));
    rtl::label(ui, tr(lang, "Line insets"));
    let columns = if ui.available_width() >= 320.0 { 4 } else { 2 };
    for start in (0..4).step_by(columns) {
        ui.columns(columns, |cols| {
            for (i, col) in (start..).zip(cols) {
                let name = tr(lang, ["Line 1", "Line 2", "Line 3", "Line 4"][i]);
                let caption = rtl::label(col, name);
                let field_id = format!("ps{prefix}ragLineInset{i}");
                if let Some(points) = NumField::measure(&field_id, stops[i], units)
                    .range(0.0, designcraft_doc::MAX_MEASURE)
                    .name(name)
                    .labelled_by(caption.id)
                    .width(col.available_width().min(100.0))
                    .show(col)
                {
                    stops[i] = Some(points);
                    // Leave the other positions alone in a mixed selection. In a paragraph
                    // style the full resolved array becomes the style's four-stop override.
                    let mut patch: [Option<f64>; 4] =
                        if prefix.is_empty() { std::array::from_fn(|j| d.fields.get(&key).and_then(|v| v[j].as_f64())) } else { stops };
                    patch[i] = Some(points);
                    edit(d, prefix, current, "ragLineInsets", json!(patch));
                }
            }
        });
    }
    if let [Some(first), Some(second), Some(third), Some(fourth)] = stops {
        stop_preview(lang, ui, [first, second, third, fourth], units);
    }
}

/// A known measure makes the preview comparable across documents regardless of panel width.
fn stop_preview(lang: &str, ui: &mut egui::Ui, stops: [f64; 4], units: Unit) {
    let measure = Unit::Millimeters.to_pt(60.0);
    let label = tr(lang, "60 mm preview");
    let t = crate::theme::Tokens::get(ui.ctx());
    rtl::label(ui, label);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width().min(210.0), 32.0), egui::Sense::hover());
    let bounds = rect.shrink(2.0);
    let painter = ui.painter();
    painter.vline(bounds.right(), bounds.y_range(), egui::Stroke::new(1.0, t.icon));
    for (i, stop) in stops.into_iter().enumerate() {
        let y = bounds.top() + (i as f32 + 0.5) * bounds.height() / 4.0;
        let end = bounds.right() - (stop.clamp(0.0, measure - 12.0) / measure) as f32 * bounds.width();
        painter.hline(bounds.left()..=end, y, egui::Stroke::new(2.0, t.text));
    }
    let measurements = stops.map(|pt| designcraft_geom::format_measure(pt, units)).join(", ");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, format!("{label}: {measurements}")));
}

/// Paragraph styles store the default language among their character attributes. The dock edits
/// the same default on selected paragraphs and keeps language overrides on individual passages.
fn language(lang: &str, ui: &mut egui::Ui, current: Option<&str>) -> Option<String> {
    let name = tr(lang, "Paragraph language");
    rtl::label(ui, name);
    let combo = craft_ui::ComboBox::from_id_salt("ragLanguage")
        .width(ui.available_width().min(210.0))
        .truncate()
        .selected_text(rtl::widget(ui, tr(lang, current.unwrap_or("Mixed"))))
        .show_ui(ui, |ui| {
            let mut picked = None;
            for &item in crate::panels::properties::LANGUAGES {
                if ui.selectable_label(current == Some(item), rtl::widget(ui, tr(lang, item))).clicked() && current != Some(item) {
                    picked = Some(item.to_owned());
                }
            }
            picked
        });
    crate::widgets::name_combo(combo, name).inner.flatten()
}

struct ParagraphFields<'a> {
    prefix: &'a str,
    para: &'a Value,
    units: Unit,
    language: Option<&'a str>,
    input_scope: u64,
}

impl ParagraphFields<'_> {
    fn show(&self, lang: &str, ui: &mut egui::Ui, d: &mut Dialog, shared: bool) -> Option<String> {
        let language = ui
            .push_id(self.input_scope, |ui| {
                show(lang, ui, d, self.prefix, self.para, self.units);
                ui.separator();
                let language = language(lang, ui, self.language);
                checkbox(lang, ui, d, self.prefix, self.para, "hyphenate", "Hyphenate");
                language
            })
            .inner;
        advanced_fields(lang, ui, d, self, shared);
        if shared {
            rtl::label(ui, tr(lang, "Use a paragraph style for fitting limits and advanced hyphenation in a shared document."));
        }
        language
    }
}

pub(crate) fn style_fields(lang: &str, ui: &mut egui::Ui, d: &mut Dialog, para: &Value, chars: &Value, units: Unit) {
    let current = value(d, "c.", chars, "language").as_str().map(str::to_owned);
    let fields = ParagraphFields { prefix: "p.", para, units, language: current.as_deref(), input_scope: 0 };
    if let Some(language) = fields.show(lang, ui, d, false) {
        edit(d, "c.", chars, "language", json!(language));
    }
}

fn advanced_fields(lang: &str, ui: &mut egui::Ui, d: &mut Dialog, fields: &ParagraphFields<'_>, shared: bool) {
    let ParagraphFields { prefix, para: current, units, input_scope, .. } = *fields;
    egui::CollapsingHeader::new(rtl::widget(ui, tr(lang, "Hyphenation settings"))).id_salt(("ragHyph", prefix)).show_unindented(ui, |ui| {
        ui.add_enabled_ui(!shared && value(d, prefix, current, "hyphenate").as_bool() != Some(false), |ui| {
            ui.push_id(input_scope, |ui| hyphenation_limits(lang, ui, d, prefix, current, units));
        });
    });
    if value(d, prefix, current, "ragPattern") == "contextualIndent" {
        return;
    }
    egui::CollapsingHeader::new(rtl::widget(ui, tr(lang, "Fitting limits"))).id_salt(("ragFitting", prefix)).show_unindented(ui, |ui| {
        ui.add_enabled_ui(!shared, |ui| ui.push_id(input_scope, |ui| fitting_limits(lang, ui, d, prefix, current)));
    });
}

fn fitting_limits(lang: &str, ui: &mut egui::Ui, d: &mut Dialog, prefix: &str, current: &Value) {
    const COLUMNS: [(&str, &str); 3] = [("Min", "Minimum"), ("Desired", "Desired"), ("Max", "Maximum")];
    ui.set_max_width(ui.available_width().min(260.0));
    ui.columns(3, |cols| {
        for ((_, label), col) in COLUMNS.into_iter().zip(cols) {
            rtl::label(col, tr(lang, label));
        }
    });
    for (row, stem, (lo, hi)) in crate::dialogs::JUSTIFY_ROWS {
        rtl::label(ui, tr(lang, row));
        ui.columns(3, |cols| {
            for ((suffix, label), col) in COLUMNS.into_iter().zip(cols) {
                let key = format!("{stem}{suffix}");
                let name = format!("{} {}", tr(lang, row), tr(lang, label));
                let pct = value(d, prefix, current, &key).as_f64().map(|v| v * 100.0);
                if let Some(n) = NumField::number(&format!("rag{prefix}{key}"), pct, "%", 0)
                    .name(&name)
                    .range(lo, hi)
                    .width(col.available_width().min(80.0))
                    .show(col)
                {
                    edit(d, prefix, current, &key, json!(n / 100.0));
                }
            }
        });
    }
}

fn checkbox(lang: &str, ui: &mut egui::Ui, d: &mut Dialog, prefix: &str, current: &Value, key: &str, label: &str) {
    let v = value(d, prefix, current, key).as_bool();
    let mut on = v.unwrap_or(false);
    if ui.add(egui::Checkbox::new(&mut on, rtl::widget(ui, tr(lang, label))).indeterminate(v.is_none())).changed() {
        edit(d, prefix, current, key, json!(on));
    }
}

/// Reserve a consistent value column so long labels wrap without pushing fields off the dock.
fn number_row(ui: &mut egui::Ui, name: &str, field: NumField<'_>) -> Option<f64> {
    let field_width = (ui.available_width() * 0.35).clamp(40.0, 76.0);
    let label_width = (ui.available_width() - field_width - ui.spacing().item_spacing.x).clamp(0.0, 140.0);
    ui.horizontal(|ui| {
        let caption = ui.add_sized([label_width, crate::widgets::FIELD_H], egui::Label::new(rtl::widget(ui, name)).wrap().halign(egui::Align::Min));
        field.name(name).labelled_by(caption.id).width(field_width).show(ui)
    })
    .inner
}

fn hyphenation_limits(lang: &str, ui: &mut egui::Ui, d: &mut Dialog, prefix: &str, current: &Value, units: Unit) {
    for (key, label, lo, hi) in [
        ("hyphMinWord", "Minimum word length", 1.0, 25.0),
        ("hyphAfterFirst", "Letters before break", 1.0, 15.0),
        ("hyphBeforeLast", "Letters after break", 1.0, 15.0),
        ("hyphLimit", "Consecutive hyphens", 0.0, 25.0),
        ("hyphZone", "Hyphenation zone", 0.0, 8640.0),
        ("hyphWeight", "Fewer hyphens (0–10)", 0.0, 10.0),
    ] {
        let name = tr(lang, label);
        let key_id = format!("rag{prefix}{key}");
        let v = value(d, prefix, current, key).as_f64();
        let field = match key {
            "hyphZone" => NumField::measure(&key_id, v, units),
            "hyphWeight" => NumField::number(&key_id, v.map(|w| (w * 10.0).round()), "", 0),
            _ => NumField::number(&key_id, v, "", 0),
        };
        if let Some(n) = number_row(ui, name, field.range(lo, hi)) {
            let v = match key {
                "hyphZone" => json!(n),
                "hyphWeight" => json!(n.round() / 10.0),
                _ => json!(n.round() as u64),
            };
            edit(d, prefix, current, key, v);
        }
    }
    for (key, label) in
        [("hyphCapitalized", "Hyphenate Capitalized Words"), ("hyphLastWord", "Hyphenate Last Word"), ("hyphAcrossColumn", "Hyphenate Across Column")]
    {
        checkbox(lang, ui, d, prefix, current, key, label);
    }
}
