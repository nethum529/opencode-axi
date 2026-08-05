//! Deny-not-ask policy carried by every worker dispatch.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A marker for an authority workers never receive.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Denied;

/// The role-owned policy described by the data/state contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerPolicy {
    pub git: Denied,
    pub destructive: Denied,
    pub credentials: Denied,
    pub external_comms: Denied,
    pub write_scope: Vec<PathBuf>,
}

/// One OpenCode permission rule. Only `deny` is expressible on restricted
/// worker dispatches, preventing a headless turn from parking on an ask.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionRule {
    pub permission: String,
    pub pattern: String,
    pub action: PermissionAction,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Deny,
}

/// The concrete OpenCode ruleset derived from a [`WorkerPolicy`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PermissionProfile(pub Vec<PermissionRule>);

impl WorkerPolicy {
    #[must_use]
    pub fn restricted(write_scope: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            git: Denied,
            destructive: Denied,
            credentials: Denied,
            external_comms: Denied,
            write_scope: write_scope.into_iter().collect(),
        }
    }

    /// Builds the deny-not-ask rules sent to OpenCode. Denying all shell use
    /// closes the git, destructive-command, credential-command, and network
    /// command paths; the remaining rules close their non-shell equivalents.
    #[must_use]
    pub fn permission_profile(&self) -> PermissionProfile {
        PermissionProfile(
            [
                "bash",
                "external_directory",
                "webfetch",
                "websearch",
                "task",
            ]
            .into_iter()
            .map(|permission| PermissionRule {
                permission: permission.to_owned(),
                pattern: "*".to_owned(),
                action: PermissionAction::Deny,
            })
            .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{PermissionAction, WorkerPolicy};

    #[test]
    fn restricted_policy_is_entirely_deny_not_ask() {
        let policy = WorkerPolicy::restricted([PathBuf::from("/work")]);
        let profile = policy.permission_profile();

        assert_eq!(policy.write_scope, [PathBuf::from("/work")]);
        assert_eq!(profile.0.len(), 5);
        assert!(
            profile
                .0
                .iter()
                .all(|rule| rule.action == PermissionAction::Deny && rule.pattern == "*")
        );
    }
}
