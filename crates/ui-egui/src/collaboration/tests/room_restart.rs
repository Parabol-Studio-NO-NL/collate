use egui_kittest::kittest::{NodeT, Queryable};

use super::*;

pub(super) fn verify_fresh_room_keeps_the_previous_room(root: &Path) {
    let path = root.join("Replacement.designcraft");
    let mut app = editor(&Document::new(&NewDocument::default()), "Room source");
    app.run("file.saveAs", json!({"path": path})).unwrap();
    drop(app);

    let mut host = reopened(root, "Replacement");
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "original room starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    let prior_host = room(&host).local.as_ref().unwrap().actor;
    let original = host.run("frame.create", json!({"rect": [30, 40, 160, 200], "content": "unassigned"})).unwrap();
    let original = ItemId(original["id"].as_u64().unwrap());
    wait_for(&mut [&mut host], "original revision is journaled", |apps| room(apps[0]).revision == 1);
    let original_bounds = document(&host).item(original).unwrap().bounds();
    drop(host);

    let mut app = reopened(root, "Replacement");
    let current = app.run("frame.create", json!({"rect": [75, 55, 160, 200], "content": "unassigned"})).unwrap();
    let current = ItemId(current["id"].as_u64().unwrap());
    let current_bounds = document(&app).item(current).unwrap().bounds();
    app.run("file.save", json!({})).unwrap();
    let current_file = std::fs::read(&path).unwrap();
    assert!(app.connect_collaboration(true).is_err());
    let previous = app.collaboration.room_conflict.as_ref().unwrap().previous;
    app.start_new_room_from_current().unwrap();
    // Dropping before transport readiness leaves the previous room active.
    assert!(room(&app).invitation.is_none());
    drop(app);

    let mut app = reopened(root, "Replacement");
    app.open_collaboration_dialog();
    let mut h = egui_kittest::Harness::builder()
        .with_size(egui::vec2(900.0, 600.0))
        .build_ui_state(|ui, app: &mut DesignApp| app.show_collaboration(ui.ctx()), app);
    h.get_by_label("Start sharing").focus();
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert_eq!(h.state().collaboration.room_conflict.as_ref().unwrap().previous, previous);
    let replacement = h.get_by_label("Start a new room from this version");
    assert!(replacement.is_focused());
    assert_eq!(replacement.accesskit_node().role(), egui::accesskit::Role::Button);
    let explanation = h.get_by_label_contains("This document differs from the saved room.");
    assert_eq!(explanation.accesskit_node().live(), egui::accesskit::Live::Polite);
    assert_eq!(document(h.state()).item(current).unwrap().bounds(), current_bounds);

    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    wait_for(&mut [h.state_mut()], "replacement room starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());
    let fresh_host = room(h.state()).local.as_ref().unwrap().actor;
    assert_ne!(fresh_host, prior_host);
    assert_eq!(room(h.state()).revision, 0);
    assert_eq!(document(h.state()).item(current).unwrap().bounds(), current_bounds);
    assert_eq!(std::fs::read(&path).unwrap(), current_file);

    let mut guest = DesignApp::new(Session::new(), Services::default());
    guest.collaboration.name = "Fresh room guest".into();
    guest.collaboration.join_code = room(h.state()).invitation.clone().unwrap();
    guest.start_seed_download().unwrap();
    wait_for(&mut [h.state_mut(), &mut guest], "guest receives the current document", |apps| {
        apps[1].collaboration.room.as_ref().is_some_and(Room::can_edit)
    });
    assert_eq!(room(&guest).host, Some(fresh_host));
    assert_eq!(document(&guest).item(current).unwrap().bounds(), current_bounds);
    drop(guest);
    drop(h);

    let mut reopened_host = reopened(root, "Replacement");
    reopened_host.connect_collaboration(true).unwrap();
    assert_eq!(room(&reopened_host).storage.identity().actor(), fresh_host);
    drop(reopened_host);

    let mut standalone = reopened(root, "Replacement");
    standalone.open_saved_room_copy().unwrap();
    assert_eq!(standalone.session.documents().len(), 2);
    assert_eq!(document(&standalone).item(original).unwrap().bounds(), original_bounds);
    assert_eq!(standalone.session.documents()[0].doc.item(current).unwrap().bounds(), current_bounds);
    assert_eq!(std::fs::read(&path).unwrap(), current_file);
}
