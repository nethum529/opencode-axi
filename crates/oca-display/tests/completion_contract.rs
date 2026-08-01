use oca_display::CompletionRecord;

#[test]
fn completion_record_has_semantically_identical_toon_and_json_twins() {
    let record = CompletionRecord::new("w00001", "completed", "success").with_worktree(
        "oca/w00001",
        "deadbeef",
        "/tmp/oca/w00001",
    );

    assert_eq!(
        record.render_toon(),
        include_str!("goldens/completion-worktree.toon")
    );
    assert_eq!(
        record.render_json(),
        "{\"ref\":\"w00001\",\"state\":\"completed\",\"outcome\":\"success\",\"branch\":\"oca/w00001\",\"commit\":\"deadbeef\",\"worktree\":\"/tmp/oca/w00001\"}\n"
    );

    let toon_round_trip = CompletionRecord::parse_toon(&record.render_toon())
        .expect("TOON rendering parses as a completion record");
    let json_round_trip = CompletionRecord::parse_json(&record.render_json())
        .expect("JSON rendering parses as a completion record");
    assert_eq!(toon_round_trip, json_round_trip);
}

#[test]
fn completion_golden_omits_worktree_metadata_for_a_non_worktree_dispatch() {
    let record = CompletionRecord::new("w00002", "completed", "success");

    assert_eq!(
        record.render_toon(),
        include_str!("goldens/completion-direct.toon")
    );
    assert_eq!(
        record.render_json(),
        "{\"ref\":\"w00002\",\"state\":\"completed\",\"outcome\":\"success\"}\n"
    );
}
