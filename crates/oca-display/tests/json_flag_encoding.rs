//! Cut-level acceptance test backfilled by the QA gate for T08 (#8).

use oca_core::{ModelCatalog, resolve_model};
use oca_display::Acknowledgement;
use serde_json::Value;

/// T08 (#8), criterion 5: `--json` changes the acknowledgement's encoding and
/// nothing else. The three fields and their values are identical in both
/// renderings, and the default rendering stays one line.
#[test]
fn json_flag_changes_only_the_acknowledgement_encoding_not_its_content() {
    let catalog = ModelCatalog::default();
    let alias = catalog
        .aliases()
        .next()
        .expect("the default catalog has aliases")
        .to_owned();
    let effort = "high";
    let resolved = resolve_model(&alias, effort, &catalog).expect("the alias resolves at high");
    let acknowledgement = Acknowledgement::from_resolved("w00001", "accepted", &resolved);

    let default_rendering = acknowledgement.render_toon();
    let json_rendering = acknowledgement.render_json();

    let fields: Value = serde_json::from_str(&json_rendering).expect("the JSON twin parses");
    let object = fields.as_object().expect("the JSON twin is an object");
    assert_eq!(object.len(), 3, "the acknowledgement carries three fields");

    let words: Vec<&str> = default_rendering.trim_end().split(' ').collect();
    assert_eq!(default_rendering.lines().count(), 1);
    assert_eq!(
        words,
        vec![
            object["ref"].as_str().expect("ref is a string"),
            object["state"].as_str().expect("state is a string"),
            object["model"].as_str().expect("model is a string"),
        ],
        "both encodings must carry the same three values"
    );
}
