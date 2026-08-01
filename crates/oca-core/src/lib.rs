pub mod error;
pub mod reply;
pub mod resolver;

pub use error::{
    ERROR_ENVELOPE_SCHEMA, ErrorCode, ErrorEnvelope, FollowExit, OcaError, error_envelope_schema,
    exit, parse_error_envelope, validate_error_envelope,
};
pub use reply::{
    ImplReply, ReviewFinding, ReviewReply, RoleReply, WorkerState, validate_reply_floor,
};
pub use resolver::{
    Alias, Catalog, Effort, EffortInput, ModelCatalog, ModelDefinition, ModelEntry, ModelSpec,
    ResolvedModel, resolve_model,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_catalog_exposes_four_aliases_and_flash_synonym() {
        let catalog = ModelCatalog::default();

        assert_eq!(
            catalog.aliases().collect::<Vec<_>>(),
            ["flash", "haiku", "opus", "sonnet"]
        );

        let flash = resolve_model("flash", "x", &catalog).expect("flash:x should resolve");
        let deepseek = resolve_model("deepseek", "x", &catalog).expect("deepseek:x should resolve");

        assert_eq!(flash, deepseek);
        assert_eq!(flash.provider, "deepseek");
        assert_eq!(flash.model, "deepseek-v4-flash-free");
        assert_eq!(flash.variant, "max");
    }

    #[test]
    fn effort_matrix_resolves_each_default_ladder() {
        let catalog = ModelCatalog::default();

        assert_matrix(
            &catalog,
            "opus",
            [
                ("l", Ok("low")),
                ("m", Ok("medium")),
                ("h", Ok("high")),
                ("x", Ok("xhigh")),
                ("max", Ok("max")),
                ("low", Ok("low")),
                ("medium", Ok("medium")),
                ("high", Ok("high")),
                ("xhigh", Ok("xhigh")),
                ("max", Ok("max")),
            ],
        );
        assert_matrix(
            &catalog,
            "sonnet",
            [
                ("l", Ok("low")),
                ("m", Ok("medium")),
                ("h", Ok("high")),
                ("x", Ok("xhigh")),
                ("max", Ok("xhigh")),
                ("low", Ok("low")),
                ("medium", Ok("medium")),
                ("high", Ok("high")),
                ("xhigh", Ok("xhigh")),
                ("max", Ok("xhigh")),
            ],
        );
        assert_matrix(
            &catalog,
            "haiku",
            [
                ("l", Ok("low")),
                ("m", Ok("medium")),
                ("h", Ok("high")),
                ("x", Ok("high")),
                ("max", Ok("high")),
                ("low", Ok("low")),
                ("medium", Ok("medium")),
                ("high", Ok("high")),
                ("xhigh", Ok("high")),
                ("max", Ok("high")),
            ],
        );
        assert_matrix(
            &catalog,
            "flash",
            [
                ("l", Err("effort_unsupported")),
                ("m", Err("effort_unsupported")),
                ("h", Ok("high")),
                ("x", Ok("max")),
                ("max", Ok("max")),
                ("low", Err("effort_unsupported")),
                ("medium", Err("effort_unsupported")),
                ("high", Ok("high")),
                ("xhigh", Ok("max")),
                ("max", Ok("max")),
            ],
        );
    }

    #[test]
    fn validation_order_is_alias_then_missing_then_conflict_then_support() {
        let catalog = ModelCatalog::default();

        assert_eq!(
            resolve_model("missing", None::<&str>, &catalog)
                .expect_err("unknown alias must win")
                .code(),
            "alias_unknown"
        );
        assert_eq!(
            resolve_model("opus", None::<&str>, &catalog)
                .expect_err("effort is mandatory")
                .code(),
            "effort_missing"
        );
        assert_eq!(
            resolve_model(
                "opus",
                EffortInput::both(Some("low"), Some("high")),
                &catalog,
            )
            .expect_err("different effort sources must conflict")
            .code(),
            "effort_conflict"
        );
        assert_eq!(
            resolve_model("flash", "low", &catalog)
                .expect_err("flash does not support low")
                .code(),
            "effort_unsupported"
        );
    }

    #[test]
    fn equal_inline_and_flag_efforts_are_accepted_once() {
        let catalog = ModelCatalog::default();

        let resolved = resolve_model("opus", EffortInput::both(Some("h"), Some("high")), &catalog)
            .expect("equivalent effort sources are not a conflict");

        assert_eq!(resolved.variant, "high");
    }

    #[test]
    fn unsupported_effort_names_the_available_ladder() {
        let catalog = ModelCatalog::default();

        let error =
            resolve_model("flash", "medium", &catalog).expect_err("medium is below flash's ladder");

        assert_eq!(error.code(), "effort_unsupported");
        assert!(error.to_string().contains("high, max"));
    }

    fn assert_matrix<const N: usize>(
        catalog: &ModelCatalog,
        alias: &str,
        cases: [(&str, Result<&str, &str>); N],
    ) {
        for (effort, expected) in cases {
            let actual = resolve_model(alias, effort, catalog);

            match expected {
                Ok(variant) => assert_eq!(actual.expect("effort should resolve").variant, variant),
                Err(code) => {
                    assert_eq!(actual.expect_err("effort should be rejected").code(), code);
                }
            }
        }
    }
}
