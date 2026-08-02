use oca_core::{ModelCatalog, resolve_model};
use oca_display::Acknowledgement;

#[test]
fn acknowledgement_default_rendering_is_the_frozen_four_alias_golden() {
    let catalog = ModelCatalog::default();
    let cases = [
        (
            "luna",
            "low",
            "accepted",
            "openai/gpt-5.6-luna:low",
            include_str!("goldens/ack-luna-accepted.toon"),
        ),
        (
            "sol",
            "medium",
            "queued",
            "openai/gpt-5.6-sol:medium",
            include_str!("goldens/ack-sol-queued.toon"),
        ),
        (
            "terra",
            "high",
            "running",
            "openai/gpt-5.6-terra:high",
            include_str!("goldens/ack-terra-running.toon"),
        ),
        (
            "flash",
            "high",
            "accepted",
            "opencode/deepseek-v4-flash-free:high",
            include_str!("goldens/ack-flash-accepted.toon"),
        ),
    ];

    for (index, (alias, effort, state, model, golden)) in cases.into_iter().enumerate() {
        let resolved = resolve_model(alias, effort, &catalog).expect("model resolves");
        let acknowledgement =
            Acknowledgement::from_resolved(format!("w0000{}", index + 1), state, &resolved);

        assert_eq!(acknowledgement.render_toon(), golden,);
        assert_eq!(
            acknowledgement.render_json(),
            format!(
                "{{\"ref\":\"w0000{}\",\"state\":\"{state}\",\"model\":\"{model}\"}}\n",
                index + 1
            ),
        );
    }
}
