use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const ERROR_ENVELOPE_SCHEMA: &str =
    include_str!("../../../compatibility/error-envelope.schema.json");

/// The process exit codes that are shared by every `oca` command.
pub mod exit {
    pub const SUCCESS: i32 = 0;
    pub const FAILURE: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const FOLLOW_BLOCKED: i32 = 3;
    pub const FOLLOW_TIMEOUT: i32 = 4;
    pub const SERVER_UNREACHABLE: i32 = 5;
    pub const INTERRUPTED: i32 = 130;
}

/// Exit outcomes emitted by the `oca f` follow command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FollowExit {
    Success,
    Blocked,
    Timeout,
    ServerUnreachable,
}

impl FollowExit {
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::Success => exit::SUCCESS,
            Self::Blocked => exit::FOLLOW_BLOCKED,
            Self::Timeout => exit::FOLLOW_TIMEOUT,
            Self::ServerUnreachable => exit::SERVER_UNREACHABLE,
        }
    }
}

/// Stable machine-readable error identifiers emitted by `oca`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    Usage,
    AliasUnknown,
    EffortMissing,
    EffortConflict,
    EffortUnsupported,
    UnknownRef,
    WorkerBusy,
    ServerUnavailable,
    ServerUnreachable,
    ProtocolMismatch,
    RateLimited,
    ContractInvalid,
    WorktreeConflict,
    WorktreeEmpty,
    ZeroByteOutput,
    PublishDisabled,
    ProtectedBranch,
    PublishRemoteMissing,
    PublishFailed,
    EventsCorrupt,
    Interrupted,
}

impl ErrorCode {
    /// The complete table of codes owned by `oca-core`.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Usage,
            Self::AliasUnknown,
            Self::EffortMissing,
            Self::EffortConflict,
            Self::EffortUnsupported,
            Self::UnknownRef,
            Self::WorkerBusy,
            Self::ServerUnavailable,
            Self::ServerUnreachable,
            Self::ProtocolMismatch,
            Self::RateLimited,
            Self::ContractInvalid,
            Self::WorktreeConflict,
            Self::WorktreeEmpty,
            Self::ZeroByteOutput,
            Self::PublishDisabled,
            Self::ProtectedBranch,
            Self::PublishRemoteMissing,
            Self::PublishFailed,
            Self::EventsCorrupt,
            Self::Interrupted,
        ]
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::AliasUnknown => "alias_unknown",
            Self::EffortMissing => "effort_missing",
            Self::EffortConflict => "effort_conflict",
            Self::EffortUnsupported => "effort_unsupported",
            Self::UnknownRef => "unknown_ref",
            Self::WorkerBusy => "worker_busy",
            Self::ServerUnavailable => "server_unavailable",
            Self::ServerUnreachable => "server_unreachable",
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::RateLimited => "rate_limited",
            Self::ContractInvalid => "contract_invalid",
            Self::WorktreeConflict => "worktree_conflict",
            Self::WorktreeEmpty => "worktree_empty",
            Self::ZeroByteOutput => "zero_byte_output",
            Self::PublishDisabled => "publish_disabled",
            Self::ProtectedBranch => "protected_branch",
            Self::PublishRemoteMissing => "publish_remote_missing",
            Self::PublishFailed => "publish_failed",
            Self::EventsCorrupt => "events_corrupt",
            Self::Interrupted => "interrupted",
        }
    }

    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "usage" => Some(Self::Usage),
            "alias_unknown" | "unknown_alias" => Some(Self::AliasUnknown),
            "effort_missing" | "missing_effort" => Some(Self::EffortMissing),
            "effort_conflict" => Some(Self::EffortConflict),
            "effort_unsupported" => Some(Self::EffortUnsupported),
            "unknown_ref" => Some(Self::UnknownRef),
            "worker_busy" => Some(Self::WorkerBusy),
            "server_unavailable" => Some(Self::ServerUnavailable),
            "server_unreachable" => Some(Self::ServerUnreachable),
            "protocol_mismatch" => Some(Self::ProtocolMismatch),
            "rate_limited" => Some(Self::RateLimited),
            "contract_invalid" => Some(Self::ContractInvalid),
            "worktree_conflict" => Some(Self::WorktreeConflict),
            "worktree_empty" => Some(Self::WorktreeEmpty),
            "zero_byte_output" => Some(Self::ZeroByteOutput),
            "publish_disabled" => Some(Self::PublishDisabled),
            "protected_branch" => Some(Self::ProtectedBranch),
            "publish_remote_missing" => Some(Self::PublishRemoteMissing),
            "publish_failed" => Some(Self::PublishFailed),
            "events_corrupt" => Some(Self::EventsCorrupt),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }

    #[must_use]
    pub const fn error(self) -> &'static str {
        match self {
            Self::Usage => "Invalid command usage",
            Self::AliasUnknown => "Unknown model alias",
            Self::EffortMissing => "Missing effort",
            Self::EffortConflict => "Conflicting effort values",
            Self::EffortUnsupported => "Unsupported effort",
            Self::UnknownRef => "Unknown ref",
            Self::WorkerBusy => "Worker is busy",
            Self::ServerUnavailable => "Server unavailable",
            Self::ServerUnreachable => "Server unreachable",
            Self::ProtocolMismatch => "Protocol mismatch",
            Self::RateLimited => "Rate limited",
            Self::ContractInvalid => "Invalid worker response",
            Self::WorktreeConflict => "Worktree conflict",
            Self::WorktreeEmpty => "Worktree is empty",
            Self::ZeroByteOutput => "Zero-byte output",
            Self::PublishDisabled => "Publishing is disabled",
            Self::ProtectedBranch => "Protected branch",
            Self::PublishRemoteMissing => "Publish remote is missing",
            Self::PublishFailed => "Publish failed",
            Self::EventsCorrupt => "Event journal is corrupt",
            Self::Interrupted => "Interrupted",
        }
    }

    #[must_use]
    pub const fn default_help(self) -> &'static str {
        match self {
            Self::Usage => "Run `oca --help` for usage",
            Self::AliasUnknown => "Use a configured model alias",
            Self::EffortMissing => "Specify an effort after the alias",
            Self::EffortConflict => "Specify effort only once",
            Self::EffortUnsupported => "Choose an effort supported by this alias",
            Self::UnknownRef => "Run `oca ls` to list refs",
            Self::WorkerBusy => "Use `oca q <ref>` to queue a message",
            Self::ServerUnavailable => "Start `opencode serve` and retry",
            Self::ServerUnreachable => "Check the OpenCode server and retry",
            Self::ProtocolMismatch => "Check the OpenCode version and retry",
            Self::RateLimited => "Wait before retrying",
            Self::ContractInvalid => "Review the worker response and retry",
            Self::WorktreeConflict => "Choose another ref or remove the conflicting worktree",
            Self::WorktreeEmpty => "Make a non-empty change before committing",
            Self::ZeroByteOutput => "Write content to every output file",
            Self::PublishDisabled => "[publish] push = true",
            Self::ProtectedBranch => "Publish from a non-protected branch",
            Self::PublishRemoteMissing => "Configure a publish remote and retry",
            Self::PublishFailed => "Inspect the remote error and retry",
            Self::EventsCorrupt => "Remove the corrupt journal after preserving its contents",
            Self::Interrupted => "Retry the command if it was not completed",
        }
    }

    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Usage
            | Self::AliasUnknown
            | Self::EffortMissing
            | Self::EffortConflict
            | Self::EffortUnsupported => exit::USAGE,
            Self::ServerUnavailable | Self::ServerUnreachable | Self::RateLimited => {
                exit::SERVER_UNREACHABLE
            }
            Self::Interrupted => exit::INTERRUPTED,
            Self::UnknownRef
            | Self::WorkerBusy
            | Self::ProtocolMismatch
            | Self::ContractInvalid
            | Self::WorktreeConflict
            | Self::WorktreeEmpty
            | Self::ZeroByteOutput
            | Self::PublishDisabled
            | Self::ProtectedBranch
            | Self::PublishRemoteMissing
            | Self::PublishFailed
            | Self::EventsCorrupt => exit::FAILURE,
        }
    }

    /// Compatibility alias for callers that used the pre-T04 spelling.
    #[allow(non_upper_case_globals)]
    pub const UnknownAlias: Self = Self::AliasUnknown;

    /// Compatibility alias for callers that used the pre-T04 spelling.
    #[allow(non_upper_case_globals)]
    pub const MissingEffort: Self = Self::EffortMissing;
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ErrorCode {
    type Err = ();

    fn from_str(code: &str) -> Result<Self, Self::Err> {
        Self::from_code(code).ok_or(())
    }
}

/// The serialized error document shared by JSON and TOON renderers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorEnvelope {
    error: String,
    code: String,
    help: String,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u64>,
}

impl ErrorEnvelope {
    #[must_use]
    pub fn error(&self) -> &str {
        &self.error
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub fn help(&self) -> &str {
        &self.help
    }

    #[must_use]
    pub fn reference(&self) -> Option<&str> {
        self.reference.as_deref()
    }

    #[must_use]
    pub const fn retry_after_ms(&self) -> Option<u64> {
        self.retry_after_ms
    }
}

/// A structured failure that can be rendered exactly once by the CLI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OcaError {
    code: ErrorCode,
    envelope: ErrorEnvelope,
}

impl OcaError {
    #[must_use]
    pub fn new(code: ErrorCode) -> Self {
        Self {
            envelope: ErrorEnvelope {
                error: code.error().to_owned(),
                code: code.as_str().to_owned(),
                help: code.default_help().to_owned(),
                reference: None,
                retry_after_ms: None,
            },
            code,
        }
    }

    #[must_use]
    pub fn publish_disabled(config_line: impl Into<String>) -> Self {
        Self::new(ErrorCode::PublishDisabled).with_help(config_line)
    }

    #[must_use]
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.envelope.error = error.into();
        self
    }

    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.envelope.help = help.into();
        self
    }

    #[must_use]
    pub fn with_ref(mut self, reference: impl Into<String>) -> Self {
        self.envelope.reference = Some(reference.into());
        self
    }

    #[must_use]
    pub const fn with_retry_after_ms(mut self, retry_after_ms: u64) -> Self {
        self.envelope.retry_after_ms = Some(retry_after_ms);
        self
    }

    #[must_use]
    pub const fn code_kind(&self) -> ErrorCode {
        self.code
    }

    #[must_use]
    pub fn code(&self) -> &str {
        self.envelope.code()
    }

    #[must_use]
    pub fn error(&self) -> &str {
        self.envelope.error()
    }

    #[must_use]
    pub fn help(&self) -> &str {
        self.envelope.help()
    }

    #[must_use]
    pub fn reference(&self) -> Option<&str> {
        self.envelope.reference()
    }

    #[must_use]
    pub const fn retry_after_ms(&self) -> Option<u64> {
        self.envelope.retry_after_ms()
    }

    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        self.code.exit_code()
    }

    #[must_use]
    pub fn envelope(&self) -> &ErrorEnvelope {
        &self.envelope
    }

    ///
    /// # Panics
    ///
    /// This cannot panic because the envelope contains only serializable primitives.
    #[must_use]
    pub fn to_json_value(&self) -> Value {
        serde_json::to_value(&self.envelope).expect("error envelope serialization cannot fail")
    }

    ///
    /// # Panics
    ///
    /// This cannot panic because the envelope contains only serializable primitives.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.envelope).expect("error envelope serialization cannot fail")
    }

    /// Render the default line-oriented, TOON-compatible representation.
    #[must_use]
    pub fn to_toon(&self) -> String {
        let mut fields = vec![
            format!("error: {}", toon_scalar(&self.envelope.error)),
            format!("code: {}", toon_scalar(&self.envelope.code)),
            format!("help: {}", toon_scalar(&self.envelope.help)),
        ];
        if let Some(reference) = &self.envelope.reference {
            fields.push(format!("ref: {}", toon_scalar(reference)));
        }
        if let Some(retry_after_ms) = self.envelope.retry_after_ms {
            fields.push(format!("retry_after_ms: {retry_after_ms}"));
        }
        fields.join("\n")
    }

    /// The sole output document for a failure; callers should not add diagnostics after it.
    #[must_use]
    pub fn render_failure(&self) -> String {
        format!("{}\n", self.to_toon())
    }
}

impl fmt::Display for OcaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_toon())
    }
}

impl Serialize for OcaError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.envelope.serialize(serializer)
    }
}

impl std::error::Error for OcaError {}

fn toon_scalar(value: &str) -> String {
    let is_safe = !value.is_empty()
        && value.trim() == value
        && !matches!(value, "true" | "false" | "null")
        && !value.starts_with('#')
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || " _./-`<>=@+*!?'".contains(character)
        });
    if is_safe {
        value.to_owned()
    } else {
        serde_json::to_string(value).expect("string serialization cannot fail")
    }
}

/// Validate an arbitrary JSON value against the error-envelope contract.
///
/// # Errors
///
/// Returns a diagnostic when the value is not an object matching the envelope shape.
pub fn validate_error_envelope(value: &Value) -> Result<(), String> {
    const ALLOWED: [&str; 5] = ["error", "code", "help", "ref", "retry_after_ms"];
    const REQUIRED: [&str; 3] = ["error", "code", "help"];

    let object = value
        .as_object()
        .ok_or_else(|| "error envelope must be a JSON object".to_owned())?;

    for field in REQUIRED {
        let value = object
            .get(field)
            .ok_or_else(|| format!("error envelope is missing `{field}`"))?;
        if value.as_str().is_none_or(str::is_empty) {
            return Err(format!(
                "error envelope `{field}` must be a non-empty string"
            ));
        }
    }

    if let Some(reference) = object.get("ref")
        && reference.as_str().is_none()
    {
        return Err("error envelope `ref` must be a string".to_owned());
    }
    if let Some(retry_after_ms) = object.get("retry_after_ms")
        && retry_after_ms.as_u64().is_none()
    {
        return Err("error envelope `retry_after_ms` must be a non-negative integer".to_owned());
    }

    if let Some(unknown) = object.keys().find(|key| !ALLOWED.contains(&key.as_str())) {
        return Err(format!("error envelope contains unknown field `{unknown}`"));
    }

    Ok(())
}

/// Validate the serialized envelope and return the parsed document.
///
/// # Errors
///
/// Returns a diagnostic when the input is not valid JSON or does not match the envelope shape.
pub fn parse_error_envelope(json: &str) -> Result<ErrorEnvelope, String> {
    let value: Value = serde_json::from_str(json).map_err(|error| error.to_string())?;
    validate_error_envelope(&value)?;
    let Some(object) = value.as_object() else {
        return Err("error envelope must be a JSON object".to_owned());
    };
    let required = |field: &str| {
        object
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("error envelope is missing `{field}`"))
    };
    Ok(ErrorEnvelope {
        error: required("error")?,
        code: required("code")?,
        help: required("help")?,
        reference: object.get("ref").and_then(Value::as_str).map(str::to_owned),
        retry_after_ms: object.get("retry_after_ms").and_then(Value::as_u64),
    })
}

/// Build the schema document used by the CLI output contract.
#[must_use]
pub fn error_envelope_schema() -> Value {
    let mut properties = Map::new();
    properties.insert(
        "error".to_owned(),
        serde_json::json!({"type": "string", "minLength": 1}),
    );
    properties.insert(
        "code".to_owned(),
        serde_json::json!({"type": "string", "minLength": 1}),
    );
    properties.insert(
        "help".to_owned(),
        serde_json::json!({"type": "string", "minLength": 1}),
    );
    properties.insert("ref".to_owned(), serde_json::json!({"type": "string"}));
    properties.insert(
        "retry_after_ms".to_owned(),
        serde_json::json!({"type": "integer", "minimum": 0}),
    );
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "oca error envelope",
        "type": "object",
        "additionalProperties": false,
        "required": ["error", "code", "help"],
        "properties": properties,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        ErrorCode, FollowExit, OcaError, error_envelope_schema, parse_error_envelope,
        validate_error_envelope,
    };

    #[test]
    fn error_metadata_and_json_envelope_are_frozen() {
        let error = OcaError::publish_disabled("[publish] push = true");

        assert_eq!(error.code(), "publish_disabled");
        assert_eq!(error.error(), "Publishing is disabled");
        assert_eq!(error.help(), "[publish] push = true");
        assert_eq!(error.exit_code(), 1);
        assert_eq!(
            error.to_json(),
            r#"{"error":"Publishing is disabled","code":"publish_disabled","help":"[publish] push = true"}"#
        );
        assert_eq!(
            serde_json::to_string(&error).expect("error envelope serialization cannot fail"),
            error.to_json()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_code_has_a_golden_envelope_and_frozen_exit_code() {
        let expected = [
            (
                "usage",
                "Invalid command usage",
                "Run `oca --help` for usage",
                2,
            ),
            (
                "alias_unknown",
                "Unknown model alias",
                "Use a configured model alias",
                2,
            ),
            (
                "effort_missing",
                "Missing effort",
                "Specify an effort after the alias",
                2,
            ),
            (
                "effort_conflict",
                "Conflicting effort values",
                "Specify effort only once",
                2,
            ),
            (
                "effort_unsupported",
                "Unsupported effort",
                "Choose an effort supported by this alias",
                2,
            ),
            ("unknown_ref", "Unknown ref", "Run `oca ls` to list refs", 1),
            (
                "worker_busy",
                "Worker is busy",
                "Use `oca q <ref>` to queue a message",
                1,
            ),
            (
                "server_unavailable",
                "Server unavailable",
                "Start `opencode serve` and retry",
                5,
            ),
            (
                "server_unreachable",
                "Server unreachable",
                "Check the OpenCode server and retry",
                5,
            ),
            (
                "protocol_mismatch",
                "Protocol mismatch",
                "Check the OpenCode version and retry",
                1,
            ),
            ("rate_limited", "Rate limited", "Wait before retrying", 5),
            (
                "contract_invalid",
                "Invalid worker response",
                "Review the worker response and retry",
                1,
            ),
            (
                "worktree_conflict",
                "Worktree conflict",
                "Choose another ref or remove the conflicting worktree",
                1,
            ),
            (
                "worktree_empty",
                "Worktree is empty",
                "Make a non-empty change before committing",
                1,
            ),
            (
                "zero_byte_output",
                "Zero-byte output",
                "Write content to every output file",
                1,
            ),
            (
                "publish_disabled",
                "Publishing is disabled",
                "[publish] push = true",
                1,
            ),
            (
                "protected_branch",
                "Protected branch",
                "Publish from a non-protected branch",
                1,
            ),
            (
                "publish_remote_missing",
                "Publish remote is missing",
                "Configure a publish remote and retry",
                1,
            ),
            (
                "publish_failed",
                "Publish failed",
                "Inspect the remote error and retry",
                1,
            ),
            (
                "events_corrupt",
                "Event journal is corrupt",
                "Remove the corrupt journal after preserving its contents",
                1,
            ),
            (
                "interrupted",
                "Interrupted",
                "Retry the command if it was not completed",
                130,
            ),
        ];

        assert_eq!(ErrorCode::all().len(), expected.len());
        for (code, expected_code) in ErrorCode::all().iter().zip(expected) {
            let value = if *code == ErrorCode::PublishDisabled {
                OcaError::publish_disabled(expected_code.2)
            } else {
                OcaError::new(*code)
            };
            assert_eq!(
                (value.code(), value.error(), value.help(), value.exit_code()),
                expected_code,
                "golden drift for {}",
                code.as_str()
            );
            assert_eq!(
                value.to_json(),
                format!(
                    "{{\"error\":{},\"code\":{},\"help\":{}}}",
                    serde_json::to_string(expected_code.1)
                        .expect("golden error serialization cannot fail"),
                    serde_json::to_string(expected_code.0)
                        .expect("golden code serialization cannot fail"),
                    serde_json::to_string(expected_code.2)
                        .expect("golden help serialization cannot fail"),
                ),
                "serialized golden drift for {}",
                code.as_str()
            );
            assert!(parse_error_envelope(&value.to_json()).is_ok());
        }
    }

    #[test]
    fn optional_fields_are_omitted_until_set_and_then_keep_their_shape() {
        let error = OcaError::new(ErrorCode::RateLimited)
            .with_ref("wabc12")
            .with_retry_after_ms(1_500);

        assert_eq!(
            error.to_json(),
            r#"{"error":"Rate limited","code":"rate_limited","help":"Wait before retrying","ref":"wabc12","retry_after_ms":1500}"#
        );
        let envelope = parse_error_envelope(&error.to_json()).expect("valid envelope");
        assert_eq!(envelope.reference(), Some("wabc12"));
        assert_eq!(envelope.retry_after_ms(), Some(1_500));
    }

    #[test]
    fn schema_requires_the_three_non_empty_fields_and_rejects_unknown_fields() {
        let schema = error_envelope_schema();
        let file_schema: serde_json::Value = serde_json::from_str(super::ERROR_ENVELOPE_SCHEMA)
            .expect("schema artifact is valid JSON");
        assert_eq!(file_schema, schema);
        assert_eq!(schema["required"], json!(["error", "code", "help"]));
        assert_eq!(schema["additionalProperties"], json!(false));

        let valid = json!({"error": "message", "code": "some_code", "help": "recover"});
        assert!(validate_error_envelope(&valid).is_ok());

        for invalid in [
            json!({"code": "some_code", "help": "recover"}),
            json!({"error": "message", "help": "recover"}),
            json!({"error": "message", "code": "some_code"}),
            json!({"error": "message", "code": "", "help": "recover"}),
            json!({"error": "message", "code": "some_code", "help": "recover", "extra": true}),
        ] {
            assert!(
                validate_error_envelope(&invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn reserved_exit_codes_are_exclusive_to_follow() {
        for code in ErrorCode::all() {
            let exit_code = code.exit_code();
            assert!(matches!(exit_code, 0 | 1 | 2 | 5 | 130));
            assert!(!matches!(exit_code, 3 | 4));
        }
        assert_eq!(FollowExit::Blocked.code(), 3);
        assert_eq!(FollowExit::Timeout.code(), 4);
    }

    #[test]
    fn single_envelope_failure_golden_captures_complete_output() {
        let error = OcaError::new(ErrorCode::UnknownRef).with_ref("wabc12");
        let captured_output = error.render_failure();

        assert_eq!(
            captured_output.as_bytes(),
            b"error: Unknown ref\ncode: unknown_ref\nhelp: Run `oca ls` to list refs\nref: wabc12\n"
        );
        assert_eq!(captured_output.matches("error:").count(), 1);
        assert_eq!(captured_output.lines().count(), 4);
    }
}
