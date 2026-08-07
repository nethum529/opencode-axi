//! Single-source rendering and composition for role prompt preambles.

use std::{collections::BTreeMap, sync::LazyLock};

/// The non-negotiable exemption included unchanged in every role preamble.
pub const STYLE_EXEMPTION: &str = "Global style or length rules for user-facing chat do not apply to this session.\nWrite your reply at the length the reply contract needs — no shorter.";

/// The role-specific data interpolated into the common worker guidance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RolePreamble {
    name: String,
    scope: String,
    fields: Vec<String>,
}

impl RolePreamble {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        scope: impl Into<String>,
        fields: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            scope: scope.into(),
            fields: fields.into_iter().map(Into::into).collect(),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn fields(&self) -> impl Iterator<Item = &str> {
        self.fields.iter().map(String::as_str)
    }
}

/// Renders one preamble for each role from the shared guidance source.
///
/// A `BTreeMap` makes generated artifact order deterministic for callers that
/// write the returned preambles to files.
#[must_use]
pub fn render_role_preambles(roles: &[RolePreamble]) -> BTreeMap<String, String> {
    roles
        .iter()
        .map(|role| (role.name.clone(), render_role_preamble(role)))
        .collect()
}

/// Returns the renderer-produced default for a built-in worker role.
///
/// The strings are compiled into every `oca` binary through `oca-core`; a bare
/// host therefore needs no installed role file. Unsupported roles have no
/// implicit contract and must provide their own preamble once supported by the
/// reply decoder.
#[must_use]
pub fn default_role_preamble(role: &str) -> Option<&'static str> {
    static IMPL: LazyLock<String> = LazyLock::new(|| {
        render_role_preamble(&RolePreamble::new(
            "impl",
            "the worker cwd",
            ["status", "files", "note"],
        ))
    });
    static REVIEW: LazyLock<String> = LazyLock::new(|| {
        render_role_preamble(&RolePreamble::new(
            "review",
            "the worker cwd",
            ["status", "findings", "note"],
        ))
    });

    match role {
        "impl" => Some(IMPL.as_str()),
        "review" => Some(REVIEW.as_str()),
        _ => None,
    }
}

/// Prefixes one task with its text-transport role contract.
#[must_use]
pub fn compose_text_prompt(preamble: &str, task: &str) -> String {
    let mut prompt = String::with_capacity(preamble.len() + task.len() + 2);
    prompt.push_str(preamble);
    if !preamble.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push('\n');
    prompt.push_str(task);
    prompt
}

fn render_role_preamble(role: &RolePreamble) -> String {
    let fields = role.fields.join(", ");
    let impl_cap = (role.name == "impl").then_some(" The `impl` note has a five-sentence cap.");

    format!(
        "# oca {role_name} role\n\n\
         ## Scope\n\
         You own only {scope}.\n\n\
         ## Denials\n\
         Git, destructive actions, credentials, and external communication are denied, not asked. \
         Report denials in the reply note.\n\n\
         ## Reply contract\n\
         Converse normally in prose so your work is visible in the TUI.\n\
         END your final message with exactly one `{role_name}` contract containing these fields: {fields}.{impl_cap}\n\
         Use the literal opening and closing fence lines shown here:\n\
         ```json\n\
         <the contract JSON>\n\
         ```\n\
         Do not place the contract JSON anywhere else in the message.\n\n\
         ## Style exemption\n\
         {STYLE_EXEMPTION}\n",
        role_name = role.name,
        scope = role.scope,
        impl_cap = impl_cap.unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::{compose_text_prompt, default_role_preamble};

    #[test]
    fn compiled_in_impl_default_comes_from_the_contract_renderer() {
        let preamble = default_role_preamble("impl").expect("impl is built in");

        assert!(preamble.contains("Use the literal opening and closing fence lines shown here:"));
        assert!(preamble.contains("\n```json\n<the contract JSON>\n```\n"));
        assert!(preamble.contains("status, files, note"));
    }

    #[test]
    fn text_composition_preserves_the_preamble_and_task_with_a_blank_separator() {
        assert_eq!(
            compose_text_prompt("replacement", "do the work"),
            "replacement\n\ndo the work"
        );
        assert_eq!(
            compose_text_prompt("replacement\n", "do the work"),
            "replacement\n\ndo the work"
        );
    }
}
