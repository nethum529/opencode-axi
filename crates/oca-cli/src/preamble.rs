//! Runtime resolution for text-transport role preambles.

use std::path::{Path, PathBuf};

use oca_core::{ErrorCode, OcaError, default_role_preamble};
use oca_state::{DispatchTransport, OcaConfig};

/// Resolves the preamble carried by one outgoing text prompt.
///
/// Schema transport retains its server-side format contract and deliberately
/// ignores role preambles. In text mode, a configured file fully replaces the
/// compiled-in renderer output.
pub(crate) fn resolve_text_preamble(
    config: &OcaConfig,
    role: &str,
    home: &Path,
) -> Result<Option<String>, OcaError> {
    if config.dispatch.transport == DispatchTransport::Schema {
        return Ok(None);
    }

    let role_config = config.roles.get(role).ok_or_else(|| {
        OcaError::new(ErrorCode::Usage)
            .with_error(format!("unknown worker role `{role}`"))
            .with_help("configure the role under [roles] and retry")
    })?;
    let Some(configured_path) = role_config.preamble_file.as_deref() else {
        return default_role_preamble(role)
            .map(|preamble| Some(preamble.to_owned()))
            .ok_or_else(|| {
                OcaError::new(ErrorCode::Usage)
                    .with_error(format!(
                        "worker role `{role}` has no compiled-in text preamble"
                    ))
                    .with_help(format!(
                        "set roles.{role}.preamble_file to a readable replacement"
                    ))
            });
    };

    let path = expand_home(configured_path, home);
    std::fs::read_to_string(&path).map(Some).map_err(|error| {
        OcaError::new(ErrorCode::Usage)
            .with_error(format!(
                "could not read roles.{role}.preamble_file `{configured_path}`: {error}"
            ))
            .with_help(format!(
                "fix `{}` or unset roles.{role}.preamble_file to use the compiled-in default",
                path.display()
            ))
    })
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_owned()
    } else if let Some(relative) = path.strip_prefix("~/") {
        home.join(relative)
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{expand_home, resolve_text_preamble};
    use oca_core::ErrorCode;
    use oca_state::OcaConfig;
    use std::path::Path;

    #[test]
    fn only_a_leading_home_component_is_expanded() {
        let home = Path::new("/test/home");

        assert_eq!(expand_home("~", home), home);
        assert_eq!(
            expand_home("~/.oca/impl.md", home),
            home.join(".oca/impl.md")
        );
        assert_eq!(
            expand_home("relative/impl.md", home),
            Path::new("relative/impl.md")
        );
    }

    #[test]
    fn unset_uses_the_compiled_in_impl_contract() {
        let home = tempfile::tempdir().expect("temporary home");
        let config = OcaConfig::from_toml_str("").expect("default config");

        let preamble = resolve_text_preamble(&config, "impl", home.path())
            .expect("compiled default resolves")
            .expect("text transport has a preamble");

        assert!(preamble.contains("\n```json\n<the contract JSON>\n```\n"));
    }

    #[test]
    fn readable_override_is_a_full_replacement() {
        let home = tempfile::tempdir().expect("temporary home");
        std::fs::write(home.path().join("replacement.md"), "replacement only")
            .expect("replacement preamble");
        let config =
            OcaConfig::from_toml_str("[roles.impl]\npreamble_file = \"~/replacement.md\"\n")
                .expect("override config");

        assert_eq!(
            resolve_text_preamble(&config, "impl", home.path())
                .expect("replacement resolves")
                .as_deref(),
            Some("replacement only")
        );
    }

    #[test]
    fn missing_or_unreadable_override_fails_truthfully() {
        let home = tempfile::tempdir().expect("temporary home");
        std::fs::create_dir(home.path().join("not-a-file")).expect("directory fixture");

        for configured in ["~/missing.md", "~/not-a-file"] {
            let config = OcaConfig::from_toml_str(&format!(
                "[roles.impl]\npreamble_file = \"{configured}\"\n"
            ))
            .expect("override config");
            let error = resolve_text_preamble(&config, "impl", home.path())
                .expect_err("bad replacement must fail");

            assert_eq!(error.code_kind(), ErrorCode::Usage);
            assert!(error.to_string().contains(configured));
            assert!(error.to_string().contains("could not read"));
        }
    }

    #[test]
    fn schema_transport_ignores_even_a_missing_override() {
        let home = tempfile::tempdir().expect("temporary home");
        let config = OcaConfig::from_toml_str(
            "[dispatch]\ntransport = \"schema\"\n\n[roles.impl]\npreamble_file = \"~/missing.md\"\n",
        )
        .expect("schema config");

        assert_eq!(
            resolve_text_preamble(&config, "impl", home.path())
                .expect("schema transport ignores preambles"),
            None
        );
    }
}
