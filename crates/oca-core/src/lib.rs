use std::fmt;

pub mod background;
pub mod error;
pub mod follow;
pub mod foreground;
pub mod message_id;
pub mod policy;
pub mod reply;
pub mod reply_decode;
pub mod resolver;

pub use background::{BackgroundOutcome, BackgroundRequest, run_background};
pub use error::{
    ERROR_ENVELOPE_SCHEMA, ErrorCode, ErrorEnvelope, FollowExit, OcaError, error_envelope_schema,
    exit, parse_error_envelope, validate_error_envelope,
};
pub use follow::{
    EventJournalWriter, EventSubscription, FollowError, FollowMessage, FollowOutcome, FollowPolicy,
    FollowTarget, FollowTerminal, FollowTransport, FollowTransportError, OcaEvent,
    follow_until_terminal, follow_until_terminal_from_cursor, follow_until_terminal_with_policy,
};
pub use foreground::{
    DispatchPrompt, ForegroundBackend, ForegroundOutcome, ForegroundRequest, TerminalReply,
    run_foreground,
};
pub use message_id::{
    MessageIdGenerator, OPENCODE_ID_SUFFIX_WIDTH, RANDOM_SUFFIX_WIDTH, TIME_PREFIX_WIDTH,
    is_opencode_message_id,
};
pub use policy::{Denied, PermissionAction, PermissionProfile, PermissionRule, WorkerPolicy};
pub use reply::{
    ImplReply, ReviewFinding, ReviewReply, RoleReply, WorkerState, validate_reply_floor,
};
pub use reply_decode::{ReplyContract, decode_role_reply};
pub use resolver::{
    Catalog, DEFAULT_MODEL_DEFINITIONS, DefaultModelDefinition, Effort, EffortInput, ModelCatalog,
    ModelDefinition, ModelEntry, ModelSpec, ResolvedModel, normalize_alias, resolve_model,
};

/// A canonical short reference used by oca state and worktree operations.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RefId(String);

impl RefId {
    /// Creates a canonical short reference.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidRefId`] when `value` is not a canonical short reference.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidRefId> {
        let value = value.into();
        if value.len() != 6
            || !value.starts_with('w')
            || !value
                .bytes()
                .skip(1)
                .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
        {
            return Err(InvalidRefId { value });
        }
        Ok(Self(value))
    }

    /// Returns the reference text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RefId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A value that does not satisfy oca's canonical short-reference format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidRefId {
    value: String,
}

impl InvalidRefId {
    /// Returns the rejected reference text.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for InvalidRefId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid oca reference: {}", self.value)
    }
}

impl std::error::Error for InvalidRefId {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_id_accepts_canonical_values() {
        for value in ["w00000", "w4f2a1", "wzzzzz"] {
            assert!(RefId::new(value).is_ok(), "{value} should be accepted");
        }
    }

    #[test]
    fn ref_id_rejects_noncanonical_values() {
        for value in [
            "x4f2a1", "w4f2a", "w4f2a10", "w4F2a1", "w4f-a1", "w4f_a1", "wé0000",
        ] {
            assert!(RefId::new(value).is_err(), "{value} should be rejected");
        }
    }

    #[test]
    fn default_catalog_exposes_four_aliases_and_flash_synonym() {
        let catalog = ModelCatalog::default();

        assert_eq!(
            catalog.aliases().collect::<Vec<_>>(),
            ["flash", "luna", "sol", "terra"]
        );

        let flash = resolve_model("flash", "x", &catalog).expect("flash:x should resolve");
        let deepseek = resolve_model("deepseek", "x", &catalog).expect("deepseek:x should resolve");

        assert_eq!(flash, deepseek);
        assert_eq!(flash.provider, "opencode");
        assert_eq!(flash.model, "deepseek-v4-flash-free");
        assert_eq!(flash.variant, "max");
    }

    #[test]
    fn catalog_synonyms_can_be_rebuilt_and_inspected() {
        let mut catalog = ModelCatalog::default();
        catalog.clear_synonyms();
        assert_eq!(catalog.synonyms().collect::<Vec<_>>(), []);

        assert_eq!(catalog.insert_synonym("  QUICK  ", "  FlAsH "), None);
        assert_eq!(catalog.synonyms().collect::<Vec<_>>(), [("quick", "flash")]);

        let quick = resolve_model("quick", "high", &catalog).expect("new synonym should resolve");
        let flash = resolve_model("flash", "high", &catalog).expect("alias should resolve");
        assert_eq!(quick, flash);
    }

    #[test]
    fn effort_matrix_resolves_each_default_ladder() {
        let catalog = ModelCatalog::default();

        assert_matrix(
            &catalog,
            "luna",
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
            "sol",
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
            "terra",
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
            "invalid_model"
        );
        assert_eq!(
            resolve_model("luna", None::<&str>, &catalog)
                .expect_err("effort is mandatory")
                .code(),
            "effort_required"
        );
        assert_eq!(
            resolve_model(
                "luna",
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

        let resolved = resolve_model("luna", EffortInput::both(Some("h"), Some("high")), &catalog)
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
