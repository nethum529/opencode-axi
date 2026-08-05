use oca_display::{Event, EventPage, ListDocument, ListItem};

#[test]
fn list_and_event_documents_have_frozen_default_and_json_renderings() {
    let list = ListDocument::new(
        vec![ListItem::new("w00001", "completed").with_model("openai/gpt-5.6-luna:low")],
        0,
        1,
    );
    assert_eq!(list.render_toon(), include_str!("goldens/list.toon"));
    assert_eq!(
        list.render_json(),
        "{\"items\":[{\"ref\":\"w00001\",\"state\":\"completed\",\"model\":\"openai/gpt-5.6-luna:low\"}],\"cursor\":0,\"total\":1}\n"
    );

    let events = EventPage::new(
        "w00001",
        vec![Event::new(7, "session.status", "completed")],
        7,
        8,
    );
    assert_eq!(events.render_toon(), include_str!("goldens/events.toon"));
    assert_eq!(
        events.render_json(),
        "{\"ref\":\"w00001\",\"events\":[{\"sequence\":7,\"kind\":\"session.status\",\"data\":\"completed\"}],\"cursor\":7,\"total\":8}\n"
    );
}
