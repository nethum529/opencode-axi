//! Structural JSON-Schema-equivalent decoding for built-in role replies.

use serde_json::Value;

use crate::{ErrorCode, ImplReply, OcaError, ReviewReply, RoleReply};

/// The structural contract selected by a resolved role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyContract {
    Impl,
    Review,
}

impl ReplyContract {
    /// Resolves the two built-in role contracts.
    ///
    /// # Errors
    ///
    /// Returns local usage failure for a role with no compiled structural
    /// decoder. User-defined schema loading belongs to its owning ticket.
    pub fn resolve(role: &str) -> Result<Self, OcaError> {
        match role {
            "impl" => Ok(Self::Impl),
            "review" => Ok(Self::Review),
            _ => Err(OcaError::new(ErrorCode::Usage)
                .with_error(format!("unknown worker role `{role}`"))
                .with_help("use a configured role with a supported reply schema")),
        }
    }

    #[must_use]
    pub fn schema(self) -> Value {
        match self {
            Self::Impl => impl_schema(),
            Self::Review => review_schema(),
        }
    }
}

/// Decodes and validates only the structural schema. T27a's minimum-length
/// floor is deliberately not called here.
///
/// # Errors
///
/// Returns `contract_invalid` for a type, field, enum, length, item-count, or
/// path-pattern violation.
pub fn decode_role_reply(contract: ReplyContract, value: Value) -> Result<RoleReply, OcaError> {
    match contract {
        ReplyContract::Impl => {
            let reply: ImplReply = serde_json::from_value(value).map_err(structural_error)?;
            if reply.files.len() > 64 {
                return Err(invalid("`files` must contain at most 64 items"));
            }
            for (index, file) in reply.files.iter().enumerate() {
                if file.is_empty() || file.starts_with('/') || file.chars().count() > 512 {
                    return Err(invalid(format!(
                        "`files[{index}]` must be a non-absolute string of at most 512 characters"
                    )));
                }
            }
            if reply.note.chars().count() > 900 {
                return Err(invalid("`note` must contain at most 900 characters"));
            }
            Ok(reply.into())
        }
        ReplyContract::Review => {
            let reply: ReviewReply = serde_json::from_value(value).map_err(structural_error)?;
            if reply.findings.len() > 100 {
                return Err(invalid("`findings` must contain at most 100 items"));
            }
            for (index, finding) in reply.findings.iter().enumerate() {
                if finding.file.chars().count() > 512 {
                    return Err(invalid(format!(
                        "`findings[{index}].file` must contain at most 512 characters"
                    )));
                }
                if !matches!(
                    finding.severity.as_str(),
                    "blocker" | "major" | "minor" | "note"
                ) {
                    return Err(invalid(format!(
                        "`findings[{index}].severity` is not a schema enum value"
                    )));
                }
                if finding.summary.chars().count() > 200 {
                    return Err(invalid(format!(
                        "`findings[{index}].summary` must contain at most 200 characters"
                    )));
                }
                if finding.fix.chars().count() > 400 {
                    return Err(invalid(format!(
                        "`findings[{index}].fix` must contain at most 400 characters"
                    )));
                }
            }
            if reply
                .note
                .as_ref()
                .is_some_and(|note| note.chars().count() > 900)
            {
                return Err(invalid("`note` must contain at most 900 characters"));
            }
            Ok(reply.into())
        }
    }
}

fn structural_error(error: serde_json::Error) -> OcaError {
    invalid(format!("reply does not match its JSON Schema: {error}"))
}

fn invalid(message: impl Into<String>) -> OcaError {
    OcaError::new(ErrorCode::ContractInvalid).with_error(message)
}

fn impl_schema() -> Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "required": ["status", "files", "note"],
        "properties": {
            "status": { "enum": ["done", "blocked", "partial"] },
            "files": {
                "type": "array", "maxItems": 64,
                "items": { "type": "string", "maxLength": 512, "pattern": "^[^/].*" }
            },
            "note": { "type": "string", "maxLength": 900 }
        }
    })
}

fn review_schema() -> Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "required": ["status", "findings"],
        "properties": {
            "status": { "enum": ["done", "blocked", "partial"] },
            "findings": {
                "type": "array", "maxItems": 100,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["file", "line", "severity", "summary", "fix"],
                    "properties": {
                        "file": { "type": "string", "maxLength": 512 },
                        "line": { "type": "integer", "minimum": 0 },
                        "severity": { "enum": ["blocker", "major", "minor", "note"] },
                        "summary": { "type": "string", "maxLength": 200 },
                        "fix": { "type": "string", "maxLength": 400 }
                    }
                }
            },
            "note": { "type": "string", "maxLength": 900 }
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ReplyContract, decode_role_reply};
    use crate::{ErrorCode, RoleReply};

    #[test]
    fn short_but_structurally_valid_reply_passes_without_t27a_floors() {
        let decoded = decode_role_reply(
            ReplyContract::Impl,
            json!({"status":"done","files":[],"note":"Done."}),
        )
        .expect("structural decoding must not enforce the reply floor");
        assert!(matches!(decoded, RoleReply::Impl(_)));
    }

    #[test]
    fn unknown_fields_and_schema_bounds_fail_contract_invalid() {
        for value in [
            json!({"status":"done","files":[],"note":"ok","extra":true}),
            json!({"status":"done","files":["/absolute"],"note":"ok"}),
            json!({"status":"done","files":[],"note":"x".repeat(901)}),
        ] {
            let error = decode_role_reply(ReplyContract::Impl, value).unwrap_err();
            assert_eq!(error.code_kind(), ErrorCode::ContractInvalid);
        }
    }
}
