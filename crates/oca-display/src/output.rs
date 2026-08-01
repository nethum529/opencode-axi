use std::fmt::Write;

use oca_core::{OcaError, ResolvedModel, validate_error_envelope};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const OUTPUT_SCHEMA: &str = include_str!("../../../compatibility/output-schema.json");

/// The three-field acknowledgement emitted as soon as a command owns a ref.
///
/// Its default rendering is deliberately a single token-minimal line. The
/// JSON representation is the machine-readable twin of the same fields.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Acknowledgement {
    #[serde(rename = "ref")]
    reference: String,
    state: String,
    model: String,
}

/// The terminal outcome for a ref-scoped command.
///
/// Worktree dispatches carry all three git-location fields; other dispatches
/// omit all three. That all-or-nothing shape makes the output safe for simple
/// consumers to branch on.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionRecord {
    #[serde(rename = "ref")]
    reference: String,
    state: String,
    outcome: String,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    worktree: Option<WorktreeCompletion>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct WorktreeCompletion {
    branch: String,
    commit: String,
    worktree: String,
}

impl CompletionRecord {
    #[must_use]
    pub fn new(
        reference: impl Into<String>,
        state: impl Into<String>,
        outcome: impl Into<String>,
    ) -> Self {
        Self {
            reference: reference.into(),
            state: state.into(),
            outcome: outcome.into(),
            worktree: None,
        }
    }

    #[must_use]
    pub fn with_worktree(
        mut self,
        branch: impl Into<String>,
        commit: impl Into<String>,
        worktree: impl Into<String>,
    ) -> Self {
        self.worktree = Some(WorktreeCompletion {
            branch: branch.into(),
            commit: commit.into(),
            worktree: worktree.into(),
        });
        self
    }

    /// Render the default line-oriented TOON-compatible record.
    #[must_use]
    pub fn render_toon(&self) -> String {
        let mut output = format!(
            "ref: {}\nstate: {}\noutcome: {}\n",
            toon_scalar(&self.reference),
            toon_scalar(&self.state),
            toon_scalar(&self.outcome),
        );
        if let Some(worktree) = &self.worktree {
            write!(
                output,
                "branch: {}\ncommit: {}\nworktree: {}\n",
                toon_scalar(&worktree.branch),
                toon_scalar(&worktree.commit),
                toon_scalar(&worktree.worktree),
            )
            .expect("writing to a String cannot fail");
        }
        output
    }

    /// Render the JSON twin of [`Self::render_toon`].
    ///
    /// # Panics
    ///
    /// Completion records contain only serializable strings.
    #[must_use]
    pub fn render_json(&self) -> String {
        format!(
            "{}\n",
            serde_json::to_string(self).expect("completion record serialization cannot fail")
        )
    }

    /// Parse a completion record from its JSON twin.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic if the value is not a valid completion record.
    pub fn parse_json(input: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(input).map_err(|error| error.to_string())?;
        validate_completion_record(&value)?;
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    /// Parse a completion record from its line-oriented TOON-compatible form.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic for malformed, duplicate, or unknown fields.
    pub fn parse_toon(input: &str) -> Result<Self, String> {
        let mut fields = serde_json::Map::new();
        for line in input.strip_suffix('\n').unwrap_or(input).split('\n') {
            let (key, value) = line
                .split_once(": ")
                .ok_or_else(|| format!("invalid completion record line `{line}`"))?;
            if fields
                .insert(key.to_owned(), Value::String(parse_toon_scalar(value)?))
                .is_some()
            {
                return Err(format!("duplicate completion record field `{key}`"));
            }
        }
        let value = Value::Object(fields);
        validate_completion_record(&value)?;
        serde_json::from_value(value).map_err(|error| error.to_string())
    }
}

/// One row in an `oca ls` output document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListItem {
    #[serde(rename = "ref")]
    reference: String,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

impl ListItem {
    #[must_use]
    pub fn new(reference: impl Into<String>, state: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            state: state.into(),
            model: None,
        }
    }

    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

/// A cursor page emitted by `oca ls`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListDocument {
    items: Vec<ListItem>,
    cursor: u64,
    total: u64,
}

impl ListDocument {
    #[must_use]
    pub fn new(items: Vec<ListItem>, cursor: u64, total: u64) -> Self {
        Self {
            items,
            cursor,
            total,
        }
    }

    #[must_use]
    pub fn render_toon(&self) -> String {
        let columns = if self.items.iter().any(|item| item.model.is_some()) {
            "ref,state,model"
        } else {
            "ref,state"
        };
        let mut output = format!(
            "cursor: {}\ntotal: {}\nitems[{}]{{{columns}}}:\n",
            self.cursor,
            self.total,
            self.items.len()
        );
        for item in &self.items {
            write!(
                output,
                "  {},{}",
                toon_scalar(&item.reference),
                toon_scalar(&item.state)
            )
            .expect("writing to a String cannot fail");
            if columns == "ref,state,model" {
                output.push(',');
                output.push_str(&toon_scalar(item.model.as_deref().unwrap_or("")));
            }
            output.push('\n');
        }
        output
    }

    /// # Panics
    ///
    /// List documents contain only serializable values.
    #[must_use]
    pub fn render_json(&self) -> String {
        render_json(self, "list document serialization cannot fail")
    }
}

/// One cursor-addressable event from `oca events`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    sequence: u64,
    kind: String,
    data: String,
}

impl Event {
    #[must_use]
    pub fn new(sequence: u64, kind: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            sequence,
            kind: kind.into(),
            data: data.into(),
        }
    }
}

/// A cursor page emitted by `oca events <ref>`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventPage {
    #[serde(rename = "ref")]
    reference: String,
    events: Vec<Event>,
    cursor: u64,
    total: u64,
}

impl EventPage {
    #[must_use]
    pub fn new(reference: impl Into<String>, events: Vec<Event>, cursor: u64, total: u64) -> Self {
        Self {
            reference: reference.into(),
            events,
            cursor,
            total,
        }
    }

    #[must_use]
    pub fn render_toon(&self) -> String {
        let mut output = format!(
            "ref: {}\ncursor: {}\ntotal: {}\nevents[{}]{{sequence,kind,data}}:\n",
            toon_scalar(&self.reference),
            self.cursor,
            self.total,
            self.events.len()
        );
        for event in &self.events {
            writeln!(
                output,
                "  {},{},{}",
                event.sequence,
                toon_scalar(&event.kind),
                toon_scalar(&event.data)
            )
            .expect("writing to a String cannot fail");
        }
        output
    }

    /// # Panics
    ///
    /// Event pages contain only serializable values.
    #[must_use]
    pub fn render_json(&self) -> String {
        render_json(self, "event page serialization cannot fail")
    }
}

/// The single semantic output model shared by default TOON-compatible and
/// `--json` renderers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputDocument {
    Acknowledgement(Acknowledgement),
    Completion(CompletionRecord),
    List(ListDocument),
    Events(EventPage),
    Error(OcaError),
}

impl OutputDocument {
    #[must_use]
    pub fn render_toon(&self) -> String {
        match self {
            Self::Acknowledgement(document) => document.render_toon(),
            Self::Completion(document) => document.render_toon(),
            Self::List(document) => document.render_toon(),
            Self::Events(document) => document.render_toon(),
            Self::Error(document) => document.render_failure(),
        }
    }

    #[must_use]
    pub fn render_json(&self) -> String {
        match self {
            Self::Acknowledgement(document) => document.render_json(),
            Self::Completion(document) => document.render_json(),
            Self::List(document) => document.render_json(),
            Self::Events(document) => document.render_json(),
            Self::Error(document) => format!("{}\n", document.to_json()),
        }
    }
}

impl Acknowledgement {
    #[must_use]
    pub fn from_resolved(
        reference: impl Into<String>,
        state: impl Into<String>,
        model: &ResolvedModel,
    ) -> Self {
        Self {
            reference: reference.into(),
            state: state.into(),
            model: format!("{}/{}:{}", model.provider, model.model, model.variant),
        }
    }

    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    #[must_use]
    pub fn state(&self) -> &str {
        &self.state
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Render the human-default acknowledgement contract.
    #[must_use]
    pub fn render_toon(&self) -> String {
        format!("{} {} {}\n", self.reference, self.state, self.model)
    }

    /// Render the JSON twin of [`Self::render_toon`].
    ///
    /// # Panics
    ///
    /// The acknowledgement contains only serializable strings.
    #[must_use]
    pub fn render_json(&self) -> String {
        format!(
            "{}\n",
            serde_json::to_string(self).expect("acknowledgement serialization cannot fail")
        )
    }
}

/// Parse the checked-in JSON Schema used for all CLI output documents.
///
/// # Panics
///
/// The checked-in schema is maintained and tested as valid JSON.
#[must_use]
pub fn output_schema() -> Value {
    serde_json::from_str(OUTPUT_SCHEMA).expect("output schema must be valid JSON")
}

/// Validate a JSON output document against the contract represented in
/// [`OUTPUT_SCHEMA`].
///
/// This deliberately mirrors the small closed set of schema shapes so callers
/// can reject an invalid document without an additional JSON-Schema runtime.
/// The checked-in schema remains the portable contract for external consumers.
///
/// # Errors
///
/// Returns a diagnostic if the document does not match one of the five output
/// document forms.
pub fn validate_output_document(value: &Value) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| "output document must be a JSON object".to_owned())?;

    if object.contains_key("items") {
        return validate_list_document(value);
    }
    if object.contains_key("events") {
        return validate_event_page(value);
    }
    if object.contains_key("error") || object.contains_key("code") || object.contains_key("help") {
        return validate_error_envelope(value);
    }
    if object.contains_key("outcome") {
        return validate_completion_record(value);
    }
    validate_acknowledgement(value)
}

fn validate_acknowledgement(value: &Value) -> Result<(), String> {
    let object = closed_object(
        value,
        &["ref", "state", "model"],
        &["ref", "state", "model"],
    )?;
    if object.len() > 3 {
        return Err("acknowledgement may contain no more than three fields".to_owned());
    }
    for field in ["ref", "state", "model"] {
        require_non_empty_string(object, field, "acknowledgement")?;
    }
    Ok(())
}

fn validate_completion_record(value: &Value) -> Result<(), String> {
    let object = closed_object(
        value,
        &["ref", "state", "outcome", "branch", "commit", "worktree"],
        &["ref", "state", "outcome"],
    )?;
    for field in ["ref", "state", "outcome"] {
        require_non_empty_string(object, field, "completion record")?;
    }

    let worktree_fields = ["branch", "commit", "worktree"];
    let present = worktree_fields
        .iter()
        .filter(|field| object.contains_key(**field))
        .count();
    if present != 0 && present != worktree_fields.len() {
        return Err(
            "completion worktree metadata must include branch, commit, and worktree together"
                .to_owned(),
        );
    }
    if present == worktree_fields.len() {
        require_non_empty_string(object, "branch", "completion record")?;
        require_non_empty_string(object, "worktree", "completion record")?;
        let commit = require_non_empty_string(object, "commit", "completion record")?;
        if !(7..=64).contains(&commit.len())
            || !commit
                .chars()
                .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
        {
            return Err(
                "completion record `commit` must be a lowercase hexadecimal SHA".to_owned(),
            );
        }
    }
    Ok(())
}

fn validate_list_document(value: &Value) -> Result<(), String> {
    let object = closed_object(
        value,
        &["items", "cursor", "total"],
        &["items", "cursor", "total"],
    )?;
    validate_page_counters(object, "list document")?;
    let items = object["items"]
        .as_array()
        .ok_or_else(|| "list document `items` must be an array".to_owned())?;
    for item in items {
        let item = closed_object(item, &["ref", "state", "model"], &["ref", "state"])?;
        require_non_empty_string(item, "ref", "list item")?;
        require_non_empty_string(item, "state", "list item")?;
        if item.contains_key("model") {
            require_non_empty_string(item, "model", "list item")?;
        }
    }
    Ok(())
}

fn validate_event_page(value: &Value) -> Result<(), String> {
    let object = closed_object(
        value,
        &["ref", "events", "cursor", "total"],
        &["ref", "events", "cursor", "total"],
    )?;
    require_non_empty_string(object, "ref", "event page")?;
    validate_page_counters(object, "event page")?;
    let events = object["events"]
        .as_array()
        .ok_or_else(|| "event page `events` must be an array".to_owned())?;
    for event in events {
        let event = closed_object(
            event,
            &["sequence", "kind", "data"],
            &["sequence", "kind", "data"],
        )?;
        if event["sequence"].as_u64().is_none() {
            return Err("event `sequence` must be a non-negative integer".to_owned());
        }
        require_non_empty_string(event, "kind", "event")?;
        if event["data"].as_str().is_none() {
            return Err("event `data` must be a string".to_owned());
        }
    }
    Ok(())
}

fn validate_page_counters(
    object: &serde_json::Map<String, Value>,
    document: &str,
) -> Result<(), String> {
    for field in ["cursor", "total"] {
        if object[field].as_u64().is_none() {
            return Err(format!(
                "{document} `{field}` must be a non-negative integer"
            ));
        }
    }
    Ok(())
}

fn closed_object<'a>(
    value: &'a Value,
    allowed: &[&str],
    required: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "output document must be a JSON object".to_owned())?;
    for field in required {
        if !object.contains_key(*field) {
            return Err(format!("output document is missing `{field}`"));
        }
    }
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(format!("output document contains unknown field `{field}`"));
    }
    Ok(object)
}

fn require_non_empty_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    document: &str,
) -> Result<&'a str, String> {
    object[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{document} `{field}` must be a non-empty string"))
}

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

fn parse_toon_scalar(value: &str) -> Result<String, String> {
    if value.starts_with('"') {
        serde_json::from_str(value).map_err(|error| error.to_string())
    } else if value.is_empty() || value.trim() != value {
        Err("TOON scalar must be non-empty and unpadded".to_owned())
    } else {
        Ok(value.to_owned())
    }
}

fn render_json(document: &impl Serialize, message: &str) -> String {
    format!("{}\n", serde_json::to_string(document).expect(message))
}
