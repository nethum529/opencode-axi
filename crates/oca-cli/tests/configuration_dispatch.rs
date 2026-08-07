use oca_cli::{Command, parse_from_home};
use oca_core::{DEFAULT_MODEL_DEFINITIONS, ErrorCode};

fn write_config(home: &std::path::Path, contents: &str) {
    let state_dir = home.join(".oca");
    std::fs::create_dir(&state_dir).expect("state directory");
    std::fs::write(state_dir.join("config.toml"), contents).expect("configuration file");
}

#[test]
fn custom_model_uses_the_production_configuration_aware_parser() {
    let home = tempfile::tempdir().expect("temporary home");
    write_config(
        home.path(),
        r#"
[models.custom]
provider = "configured-provider"
model = "configured-model"
efforts = ["high"]
"#,
    );

    let Command::Dispatch(dispatch) =
        parse_from_home(["oca", "custom:high", "do", "the", "work"], home.path())
            .expect("configured custom model should dispatch")
    else {
        panic!("custom model must parse as dispatch");
    };

    assert_eq!(dispatch.model.alias, "custom");
    assert_eq!(dispatch.model.provider, "configured-provider");
    assert_eq!(dispatch.model.model, "configured-model");
    assert_eq!(dispatch.model.effort, "high");
    assert_eq!(dispatch.model.variant, "high");
}

#[test]
fn flash_override_controls_the_effort_ladder() {
    let home = tempfile::tempdir().expect("temporary home");
    write_config(
        home.path(),
        r#"
[models.flash]
provider = "override-provider"
model = "override-model"
efforts = ["max"]
"#,
    );

    let Command::Dispatch(dispatch) =
        parse_from_home(["oca", "flash:max", "do", "the", "work"], home.path())
            .expect("configured flash:max should dispatch")
    else {
        panic!("flash must parse as dispatch");
    };
    assert_eq!(dispatch.model.provider, "override-provider");
    assert_eq!(dispatch.model.model, "override-model");
    assert_eq!(dispatch.model.effort, "max");
    assert_eq!(dispatch.model.variant, "max");
    assert!(!dispatch.model.tooled_incompatible);

    let error = parse_from_home(["oca", "flash:high", "do", "the", "work"], home.path())
        .expect_err("the override removes high from flash's effort ladder");
    assert_eq!(error.code(), ErrorCode::EffortUnsupported.as_str());
}

#[test]
fn absent_configuration_preserves_default_dispatch() {
    let home = tempfile::tempdir().expect("temporary home");

    let error = parse_from_home(["oca", "flash:high", "do the work"], home.path())
        .expect_err("the compiled flash entry remains blocked for tooled dispatch");
    assert_eq!(error.code(), ErrorCode::ModelUnsupportedTooled.as_str());

    for definition in DEFAULT_MODEL_DEFINITIONS {
        if definition.tooled_incompatible {
            continue;
        }
        for effort in definition.ladder {
            let request = format!("{}:{effort}", definition.alias);
            let Command::Dispatch(dispatch) =
                parse_from_home(["oca", request.as_str(), "do the work"], home.path())
                    .unwrap_or_else(|error| panic!("{request} should dispatch: {error}"))
            else {
                panic!("{request} must parse as dispatch");
            };

            assert_eq!(dispatch.model.alias, definition.alias);
            assert_eq!(dispatch.model.provider, definition.provider);
            assert_eq!(dispatch.model.model, definition.model);
            assert_eq!(dispatch.model.effort, *effort);
            assert_eq!(dispatch.model.variant, *effort);
        }
    }
}
