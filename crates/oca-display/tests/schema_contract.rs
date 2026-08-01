use oca_display::{output_schema, validate_output_document};
use serde_json::json;

#[test]
fn schema_and_validator_reject_contract_violations() {
    let schema = output_schema();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(schema["$defs"]["acknowledgement"]["maxProperties"], 3);

    let ack_with_an_added_field = json!({
        "ref": "w00001",
        "state": "accepted",
        "model": "anthropic/claude-opus-4-1:low",
        "extra": "not allowed"
    });
    assert!(validate_output_document(&ack_with_an_added_field).is_err());

    let negative_cursor = json!({"items": [], "cursor": -1, "total": 0});
    assert!(validate_output_document(&negative_cursor).is_err());

    let negative_total = json!({"items": [], "cursor": 0, "total": -1});
    assert!(validate_output_document(&negative_total).is_err());

    for missing_field in ["error", "code", "help"] {
        let mut error = json!({
            "error": "Unknown ref",
            "code": "unknown_ref",
            "help": "Run `oca ls` to list refs"
        });
        error
            .as_object_mut()
            .expect("error is an object")
            .remove(missing_field);
        assert!(
            validate_output_document(&error).is_err(),
            "missing {missing_field}"
        );
    }
}
