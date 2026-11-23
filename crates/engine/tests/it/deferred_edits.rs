use designcraft_doc::{Document, ItemId, LayerId};
use designcraft_engine::{EngineError, Session};
use serde_json::json;

fn open_document() -> Session {
    let mut session = Session::new();
    session.execute("file.new", &json!({})).unwrap();
    session
}

fn enable_deferred_edits(session: &mut Session) {
    let uid = session.doc().unwrap().uid;
    session.defer_document_edits(Some(uid));
    session.set_deferred_edit_ready(true);
}

fn frame_x(doc: &Document, id: ItemId) -> f64 {
    doc.item(id).unwrap().bounds().x0
}

#[test]
fn a_direct_frame_edit_yields_one_proposal_without_committing() {
    let mut session = open_document();
    enable_deferred_edits(&mut session);
    let original = session.doc().unwrap().clone();
    let journal = session.journal.clone();
    let created = session.execute("frame.create", &json!({"rect": [20, 40, 100, 160], "content": "unassigned"})).unwrap();
    let id = ItemId(created["id"].as_u64().unwrap());
    assert!(session.has_deferred_edit());
    let confirmed = session.doc().unwrap();
    assert!(confirmed.doc.item(id).is_none());
    assert_eq!(confirmed.revision, original.revision);
    assert_eq!(confirmed.history.undo.len(), original.history.undo.len());
    assert_eq!(session.journal, journal);

    session.set_deferred_edit_ready(true);
    assert!(session.execute("frame.create", &json!({"rect": [50, 80, 150, 180], "content": "unassigned"})).is_err());
    let proposal = session.take_deferred_edit().unwrap();
    assert_eq!(proposal.before, original.doc);
    assert!(proposal.after.item(id).is_some());
    assert!(session.take_deferred_edit().is_none());
    assert!(session.execute("frame.create", &json!({"rect": [50, 80, 150, 180], "content": "unassigned"})).is_err());
    assert_eq!(session.doc().unwrap().doc, original.doc);
}

#[test]
fn a_deferred_layer_edit_preserves_confirmed_activation_and_allows_a_local_switch() {
    let mut session = open_document();
    let base = session.doc().unwrap().active_layer;
    let frame = session.execute("frame.create", &json!({"rect": [20, 40, 100, 160], "content": "unassigned"})).unwrap()["id"].as_u64().unwrap();
    let removable = LayerId(session.execute("layer.new", &json!({"name": "Empty"})).unwrap()["id"].as_u64().unwrap());
    session.execute("selection.set", &json!({"ids": [frame]})).unwrap();
    enable_deferred_edits(&mut session);
    let original = session.doc().unwrap().clone();
    let journal = session.journal.clone();

    let created = LayerId(session.execute("layer.new", &json!({"name": "Draft"})).unwrap()["id"].as_u64().unwrap());
    let confirmed = session.doc().unwrap();
    assert_eq!(confirmed.doc, original.doc);
    assert_eq!(confirmed.active_layer, removable);
    assert_eq!(confirmed.selection, original.selection);
    assert_eq!(confirmed.revision, original.revision);
    assert_eq!(session.journal, journal);

    assert!(session.execute("layer.activate", &json!({"id": created.0})).is_err());
    session.execute("layer.activate", &json!({"id": base.0})).unwrap();
    assert_eq!(session.doc().unwrap().active_layer, base);
    assert_eq!(session.doc().unwrap().doc, original.doc);
    let creation = session.take_deferred_edit().unwrap();
    assert_eq!(creation.before, original.doc);
    assert_eq!(creation.after.layer(created).unwrap().name, "Draft");

    session.set_deferred_edit_ready(true);
    session.execute("layer.activate", &json!({"id": removable.0})).unwrap();
    let before_delete = session.doc().unwrap().clone();
    session.execute("layer.delete", &json!({"id": removable.0})).unwrap();
    let confirmed = session.doc().unwrap();
    assert_eq!(confirmed.active_layer, removable);
    assert_eq!(confirmed.selection, before_delete.selection);
    assert_eq!(confirmed.revision, before_delete.revision);
    assert_eq!(confirmed.doc, before_delete.doc);
    let deletion = session.take_deferred_edit().unwrap();
    assert!(deletion.after.layer(removable).is_none());
    assert!(deletion.after_selection.items.is_empty());

    session.set_deferred_edit_ready(true);
    assert!(session.execute("layer.move", &json!({"id": removable.0, "to": 99})).is_err());
    assert!(!session.has_deferred_edit());
    assert_eq!(session.doc().unwrap().doc, before_delete.doc);
    assert_eq!(session.doc().unwrap().active_layer, removable);
}

#[test]
fn only_the_final_drag_preview_leaves_the_engine_as_a_proposal() {
    let mut session = open_document();
    let created = session.execute("frame.create", &json!({"rect": [20, 40, 100, 160], "content": "unassigned"})).unwrap();
    let id = ItemId(created["id"].as_u64().unwrap());
    enable_deferred_edits(&mut session);
    let original = session.doc().unwrap().clone();
    let original_x = frame_x(&original.doc, id);
    let journal = session.journal.clone();

    session.begin_interaction("Move").unwrap();
    session.preview("transform.move", json!({"id": id.0, "dx": 25})).unwrap();
    assert!(frame_x(&session.doc().unwrap().doc, id) > original_x);
    assert_eq!(session.doc().unwrap().interaction.as_ref().unwrap().doc, original.doc);
    assert!(!session.has_deferred_edit());

    session.preview("transform.move", json!({"id": id.0, "dx": 50})).unwrap();
    let final_preview = session.doc().unwrap().doc.clone();
    assert!(!session.has_deferred_edit());
    session.commit_interaction().unwrap();

    let confirmed = session.doc().unwrap();
    assert_eq!(confirmed.doc, original.doc);
    assert_eq!(confirmed.revision, original.revision);
    assert_eq!(confirmed.history.undo.len(), original.history.undo.len());
    assert_eq!(session.journal, journal);
    let proposal = session.take_deferred_edit().unwrap();
    assert_eq!(proposal.before, original.doc);
    assert_eq!(proposal.after, final_preview);
    assert_eq!(frame_x(&proposal.after, id) - original_x, 50.0);
}

#[test]
fn a_shared_document_rejects_unsupported_changes_then_restores_standalone_history() {
    let mut session = open_document();
    session.execute("frame.create", &json!({"rect": [20, 40, 100, 160], "content": "text", "text": "Shared story"})).unwrap();
    let original = session.doc().unwrap().clone();
    enable_deferred_edits(&mut session);

    for (command, args) in [("type.fillWithPlaceholder", json!({})), ("file.revert", json!({})), ("edit.undo", json!({})), ("edit.redo", json!({}))] {
        assert!(matches!(session.execute(command, &args), Err(EngineError::Other(_))), "{command}");
        assert_eq!(session.doc().unwrap().doc, original.doc);
        assert!(!session.has_deferred_edit());
    }

    session.defer_document_edits(None);
    session.execute("text.insert", &json!({"text": "Standalone edit"})).unwrap();
    assert_ne!(session.doc().unwrap().doc, original.doc);
    session.execute("edit.undo", &json!({})).unwrap();
    assert_eq!(session.doc().unwrap().doc, original.doc);
    assert!(!session.has_deferred_edit());
}
