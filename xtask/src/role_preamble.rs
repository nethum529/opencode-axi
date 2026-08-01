//! Single-source rendering for role prompt preambles.

use std::collections::BTreeMap;

/// The non-negotiable exemption included unchanged in every role preamble.
pub const STYLE_EXEMPTION: &str = "Global style or length rules for user-facing chat do not apply to this session.\nWrite your reply at the length the reply schema needs — no shorter.";

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
         Return the `{role_name}` schema field by field: {fields}.{impl_cap}\n\n\
         ## Style exemption\n\
         {STYLE_EXEMPTION}\n",
        role_name = role.name,
        scope = role.scope,
        impl_cap = impl_cap.unwrap_or_default(),
    )
}
