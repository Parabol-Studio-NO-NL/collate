use egui_kittest::kittest::{NodeT, Queryable};

use super::*;

#[test]
fn guest_sees_missing_fonts_on_join_and_after_peer_edits() {
    let mut host = editor(&Document::new(&NewDocument::default()), "Host");
    let created = host.run("frame.create", json!({"rect": [40, 50, 300, 200], "content": "text", "text": "AB"})).unwrap();
    let story = created["story"].as_u64().unwrap();
    host.run("text.select", json!({"story": story, "anchor": 0, "focus": 1})).unwrap();
    host.run("type.char", json!({"fontFamily": "Collate Absent Family"})).unwrap();
    host.connect_collaboration(true).unwrap();
    wait_for(&mut [&mut host], "host starts", |apps| room(apps[0]).invitation.is_some() && room(apps[0]).can_edit());

    let mut guest = DesignApp::new(Session::new(), Services::default());
    guest.collaboration.name = "Guest".into();
    guest.collaboration.join_code = room(&host).invitation.clone().unwrap();
    guest.start_seed_download().unwrap();
    wait_for(&mut [&mut host, &mut guest], "guest receives seed", |apps| apps[1].collaboration.room.as_ref().is_some_and(Room::can_edit));
    let mut h = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1000.0, 400.0))
        .with_context_setup(|ctx| ctx.set_fonts(crate::theme::font_definitions(&[], "en")))
        .build_ui_state(|ui, guest| crate::chrome::status_bar(guest, ui), guest);
    let notice = h.get_by_label("Missing fonts (1)");
    assert_eq!(notice.accesskit_node().live(), egui::accesskit::Live::Polite);
    notice.focus();
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert!(h.query_by_label("Missing font: Collate Absent Family").is_some());
    assert!(!h.get_by_label("Reload Fonts").accesskit_node().is_disabled());
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert!(room(h.state()).pending.is_none() && room(h.state()).history.is_empty());

    host.run("text.select", json!({"story": story, "anchor": 1, "focus": 2})).unwrap();
    host.run("type.char", json!({"attrs": {"fontFamily": "Source Serif 4", "fontStyle": "Collate Absent Style"}})).unwrap();
    wait_for(&mut [&mut host, h.state_mut()], "guest receives new face", |apps| apps.iter().all(|a| room(a).revision == 1));
    h.run_steps(3);
    h.get_by_label("Missing fonts (2)").click();
    h.run_steps(3);
    assert!(h.query_by_label_contains("Missing font style: Source Serif 4 Collate Absent Style").is_some());

    host.run("text.select", json!({"story": story, "anchor": 0, "focus": 2})).unwrap();
    host.run("type.char", json!({"attrs": {"fontFamily": "Source Serif 4", "fontStyle": "Regular"}})).unwrap();
    wait_for(&mut [&mut host, h.state_mut()], "guest resolves both fonts", |apps| apps.iter().all(|a| room(a).revision == 2));
    h.run_steps(3);
    assert!(h.query_by_label_contains("Missing fonts (").is_none());
}
