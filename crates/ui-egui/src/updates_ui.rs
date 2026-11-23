//! The desktop host supplies its updater. Other builds display how to get updates.

use serde_json::Value;

use crate::DesignApp;

#[derive(Default)]
pub(crate) struct Runtime {
    check: Option<Box<dyn Fn() -> Result<(), String>>>,
    manual_window_open: bool,
    focus_dismiss: bool,
}

impl DesignApp {
    pub fn set_update_checker(&mut self, check: impl Fn() -> Result<(), String> + 'static) {
        self.updates_rt.check = Some(Box::new(check));
    }
}

pub(crate) fn check(app: &mut DesignApp) -> Result<Value, String> {
    if let Some(check) = &app.updates_rt.check {
        check()?;
    } else {
        app.updates_rt.manual_window_open = true;
        app.updates_rt.focus_dismiss = true;
    }
    Ok(Value::Null)
}

pub(crate) fn show(app: &mut DesignApp, ctx: &egui::Context) {
    if !app.updates_rt.manual_window_open {
        return;
    }
    let mut open = true;
    let mut dismissed = false;
    let focus_dismiss = std::mem::take(&mut app.updates_rt.focus_dismiss);
    let title = crate::i18n::tr(&app.ui.language, "Collate Updates");
    let ok_label = crate::i18n::tr(&app.ui.language, "OK");
    let message = if cfg!(target_os = "macos") {
        "Install a release build of Collate to check for updates."
    } else {
        "Open Collate for macOS to check for updates."
    };
    egui::Window::new(title)
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
            ui.label(crate::i18n::tr(&app.ui.language, message));
            ui.add_space(8.0);
            let dismiss = ui.button(ok_label);
            if focus_dismiss {
                dismiss.request_focus();
            }
            dismissed = dismiss.clicked() || ui.input(|input| input.key_pressed(egui::Key::Escape));
        });
    app.updates_rt.manual_window_open = open && !dismissed;
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use designcraft_engine::Session;

    #[test]
    fn manual_checks_use_only_the_platform_updater() {
        let mut app = DesignApp::new(Session::new(), Default::default());
        let calls = Rc::new(Cell::new(0));
        let observed = calls.clone();
        app.set_update_checker(move || {
            observed.set(observed.get() + 1);
            Ok(())
        });
        app.run("help.checkForUpdates", Value::Null).unwrap();
        assert_eq!(calls.get(), 1);
        assert!(!app.updates_rt.manual_window_open);
    }

    #[test]
    fn builds_without_an_updater_show_installation_guidance() {
        let mut app = DesignApp::new(Session::new(), Default::default());
        app.run("help.checkForUpdates", Value::Null).unwrap();
        assert!(app.updates_rt.manual_window_open);
    }
}
