//! Role reply data and the semantic floor that follows structural decoding.

use serde::{Deserialize, Serialize};

use crate::{ErrorCode, OcaError};

/// The terminal state reported by a worker role.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerState {
    Done,
    Blocked,
    Partial,
}

/// A structurally decoded reply from the default implementation role.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImplReply {
    pub status: WorkerState,
    pub files: Vec<String>,
    pub note: String,
}

/// A structurally decoded reply from the default review role.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewReply {
    pub status: WorkerState,
    pub findings: Vec<ReviewFinding>,
    pub note: Option<String>,
}

/// One review finding reported by a worker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFinding {
    pub file: String,
    pub line: u32,
    pub severity: String,
    pub summary: String,
    pub fix: String,
}

/// A reply whose JSON shape has already passed the role's structural schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoleReply {
    Impl(ImplReply),
    Review(ReviewReply),
}

impl From<ImplReply> for RoleReply {
    fn from(reply: ImplReply) -> Self {
        Self::Impl(reply)
    }
}

impl From<ReviewReply> for RoleReply {
    fn from(reply: ReviewReply) -> Self {
        Self::Review(reply)
    }
}

/// Checks the semantic minimum-length contract after structural schema validation.
///
/// # Errors
///
/// Returns `contract_invalid` naming the field and floor that a reply misses.
/// Dispatch and commit callers must stop before any finalization when this fails.
pub fn validate_reply_floor(reply: &RoleReply) -> Result<(), OcaError> {
    match reply {
        RoleReply::Impl(reply) => validate_impl_floor(reply),
        RoleReply::Review(reply) => validate_review_floor(reply),
    }
}

fn validate_impl_floor(reply: &ImplReply) -> Result<(), OcaError> {
    match reply.status {
        WorkerState::Done | WorkerState::Partial
            if word_count(&reply.note) < 25 || sentence_count(&reply.note) < 2 =>
        {
            return Err(floor_error(
                "note",
                "at least 25 words and 2 sentences for done or partial impl replies",
            ));
        }
        WorkerState::Blocked if word_count(&reply.note) < 10 => {
            return Err(floor_error(
                "note",
                "at least 10 words for a blocked impl reply",
            ));
        }
        _ => {}
    }

    if !reply.files.is_empty() && !names_or_characterizes_change(&reply.note, &reply.files) {
        return Err(floor_error(
            "note",
            "name a changed file or characterize the change when files are reported",
        ));
    }

    Ok(())
}

fn validate_review_floor(reply: &ReviewReply) -> Result<(), OcaError> {
    for (index, finding) in reply.findings.iter().enumerate() {
        if word_count(&finding.summary) < 6 {
            return Err(floor_error(
                &format!("findings[{index}].summary"),
                "at least 6 words",
            ));
        }
        if word_count(&finding.fix) < 6 {
            return Err(floor_error(
                &format!("findings[{index}].fix"),
                "at least 6 words",
            ));
        }
    }

    if reply.findings.is_empty() && reply.status == WorkerState::Done {
        let note = reply.note.as_deref().ok_or_else(|| {
            floor_error("note", "required and at least 15 words for a clean review")
        })?;
        if word_count(note) < 15 {
            return Err(floor_error("note", "at least 15 words for a clean review"));
        }
    }

    Ok(())
}

fn floor_error(field: &str, floor: &str) -> OcaError {
    OcaError::new(ErrorCode::ContractInvalid)
        .with_error(format!("Invalid worker response: `{field}` must {floor}"))
}

fn word_count(value: &str) -> usize {
    value.split_whitespace().count()
}

fn sentence_count(value: &str) -> usize {
    let bytes = value.as_bytes();
    let mut count = 0;

    for (index, byte) in bytes.iter().enumerate() {
        if !matches!(byte, b'.' | b'?' | b'!') {
            continue;
        }
        let next_is_boundary = bytes.get(index + 1).is_none_or(u8::is_ascii_whitespace);
        if !next_is_boundary || (*byte == b'.' && is_decimal_point(bytes, index)) {
            continue;
        }
        count += 1;
    }

    count
}

fn is_decimal_point(bytes: &[u8], index: usize) -> bool {
    bytes
        .get(index.wrapping_sub(1))
        .is_some_and(u8::is_ascii_digit)
        && bytes.get(index + 1).is_some_and(u8::is_ascii_digit)
}

fn names_or_characterizes_change(note: &str, files: &[String]) -> bool {
    if files.iter().any(|file| note.contains(file)) {
        return true;
    }

    note.split(|character: char| !character.is_alphabetic())
        .any(|word| {
            matches!(
                word.to_ascii_lowercase().as_str(),
                "added"
                    | "changed"
                    | "created"
                    | "documented"
                    | "fixed"
                    | "implemented"
                    | "refactored"
                    | "removed"
                    | "renamed"
                    | "tested"
                    | "updated"
                    | "wrote"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::sentence_count;

    #[test]
    fn sentence_count_ignores_decimals() {
        assert_eq!(sentence_count("Version 1.0 is supported. Continue?"), 2);
    }
}
