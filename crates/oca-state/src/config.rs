//! Typed, durable access to the configuration that belongs to `oca`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use oca_core::{DEFAULT_MODEL_DEFINITIONS, ModelCatalog, ModelDefinition, normalize_alias};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const STATE_DIRECTORY: &str = ".oca";
const CONFIG_FILE: &str = "config.toml";

/// The configuration loaded from `~/.oca/config.toml`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct OcaConfig {
    pub schema_version: u32,
    pub models: BTreeMap<String, ModelConfig>,
    pub roles: BTreeMap<String, RoleConfig>,
    pub dispatch: DispatchConfig,
    pub herdr: HerdrConfig,
    pub publish: PublishConfig,
    pub server: ServerConfig,
    pub retention: RetentionConfig,
    #[serde(skip)]
    source: String,
}

impl Default for OcaConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            models: default_models(),
            roles: default_roles(),
            dispatch: DispatchConfig::default(),
            herdr: HerdrConfig::default(),
            publish: PublishConfig::default(),
            server: ServerConfig::default(),
            retention: RetentionConfig::default(),
            source: String::new(),
        }
    }
}

impl OcaConfig {
    /// Parses a configuration fragment over the compiled-in defaults.
    ///
    /// Model and role tables are deep-merged, so adding `[models.example]`
    /// extends the catalog rather than replacing the built-in aliases.
    ///
    /// # Errors
    ///
    /// Returns an error when the input is not valid TOML or cannot be decoded
    /// as this schema.
    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        let default_toml = toml::to_string(&Self::default())?;
        let mut defaults = toml::Value::Table(toml::from_str(&default_toml)?);
        let supplied = toml::Value::Table(toml::from_str(input)?);
        merge_tables(&mut defaults, &supplied);

        let mut config = defaults.try_into::<Self>()?;
        input.clone_into(&mut config.source);
        Ok(config)
    }

    /// Loads the user's configuration, creating `~/.oca` securely when it is
    /// absent. A missing config file is equivalent to an empty config.
    ///
    /// # Errors
    ///
    /// Returns an error when the state directory cannot be secured, the file
    /// cannot be read, or its TOML is invalid.
    pub fn load_from_home(home: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Self::load_from_state_dir(home.as_ref().join(STATE_DIRECTORY))
    }

    /// Loads configuration from an explicit state directory, primarily for
    /// callers that need an isolated state root.
    ///
    /// # Errors
    ///
    /// Returns an error when the state directory cannot be secured, the file
    /// cannot be read, or its TOML is invalid.
    pub fn load_from_state_dir(state_dir: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let state_dir = state_dir.as_ref();
        ensure_private_directory(state_dir)?;
        let config_path = state_dir.join(CONFIG_FILE);
        match fs::read_to_string(&config_path) {
            Ok(contents) => Self::from_toml_str(&contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::from_toml_str(""),
            Err(error) => Err(error.into()),
        }
    }

    /// Writes the configuration to `~/.oca/config.toml` with private
    /// permissions. Parsed source is retained so unknown keys survive a
    /// read/write round trip.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration serialization or the secure file
    /// write fails.
    pub fn write_to_home(&self, home: impl AsRef<Path>) -> Result<(), ConfigError> {
        self.write_to_state_dir(home.as_ref().join(STATE_DIRECTORY))
    }

    /// Writes the configuration to an explicit state directory.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration serialization or the secure file
    /// write fails.
    pub fn write_to_state_dir(&self, state_dir: impl AsRef<Path>) -> Result<(), ConfigError> {
        let state_dir = state_dir.as_ref();
        ensure_private_directory(state_dir)?;
        let contents = if self.source.is_empty() {
            toml::to_string_pretty(self)?
        } else {
            let mut source: toml::Value = toml::Value::Table(toml::from_str(&self.source)?);
            let current: toml::Value = toml::Value::Table(toml::from_str(&toml::to_string(self)?)?);
            merge_tables(&mut source, &current);
            toml::to_string_pretty(&source)?
        };
        Ok(write_private_file(
            &state_dir.join(CONFIG_FILE),
            contents.as_bytes(),
        )?)
    }

    /// Returns forward-compatibility warnings for an opted-in diagnostic
    /// display. Loading itself is deliberately silent.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<ConfigDiagnostic> {
        let Ok(document) = toml::from_str::<toml::Table>(&self.source) else {
            return Vec::new();
        };
        let mut diagnostics = Vec::new();

        if let Some(toml::Value::Table(models)) = document.get("models") {
            for (alias, value) in models {
                if let toml::Value::Table(model) = value {
                    collect_unknown_keys(
                        model,
                        &format!("models.{alias}"),
                        &["provider", "model", "efforts", "synonyms", "shorthand"],
                        &mut diagnostics,
                    );
                }
            }
        }
        if let Some(toml::Value::Table(roles)) = document.get("roles") {
            for (role, value) in roles {
                if let toml::Value::Table(role_config) = value {
                    collect_unknown_keys(
                        role_config,
                        &format!("roles.{role}"),
                        &["schema", "preamble_file", "permission", "min_words"],
                        &mut diagnostics,
                    );
                }
            }
        }
        collect_table_diagnostics(&document, "dispatch", &["transport"], &mut diagnostics);
        collect_table_diagnostics(
            &document,
            "herdr",
            &["workspace", "socket", "timeout_ms", "close_on_done"],
            &mut diagnostics,
        );
        collect_table_diagnostics(
            &document,
            "publish",
            &["push", "pr", "branches", "remote", "base", "overrides"],
            &mut diagnostics,
        );
        if let Some(toml::Value::Table(publish)) = document.get("publish")
            && let Some(toml::Value::Table(overrides)) = publish.get("overrides")
        {
            for (path, value) in overrides {
                if let toml::Value::Table(override_config) = value {
                    collect_unknown_keys(
                        override_config,
                        &format!("publish.overrides.{path}"),
                        &["push", "pr"],
                        &mut diagnostics,
                    );
                }
            }
        }
        collect_table_diagnostics(
            &document,
            "server",
            &["port", "alt_ports", "start_timeout_ms"],
            &mut diagnostics,
        );
        collect_table_diagnostics(
            &document,
            "retention",
            &["journal_days", "tombstone_days"],
            &mut diagnostics,
        );

        diagnostics
    }

    /// Builds the resolver catalog represented by the merged model configuration.
    #[must_use]
    pub fn model_catalog(&self) -> ModelCatalog {
        let mut catalog = ModelCatalog::default();
        catalog.clear_synonyms();
        let overridden_aliases = toml::from_str::<toml::Table>(&self.source)
            .ok()
            .and_then(|document| {
                document
                    .get("models")
                    .and_then(toml::Value::as_table)
                    .cloned()
            })
            .map(|models| models.keys().map(normalize_alias).collect::<BTreeSet<_>>())
            .unwrap_or_default();

        for (alias, model) in &self.models {
            let tooled_incompatible = !overridden_aliases.contains(&normalize_alias(alias))
                && DEFAULT_MODEL_DEFINITIONS
                    .iter()
                    .find(|definition| normalize_alias(definition.alias) == normalize_alias(alias))
                    .is_some_and(|definition| definition.tooled_incompatible);
            catalog.insert(
                alias,
                ModelDefinition::new(&model.provider, &model.model, &model.efforts)
                    .with_tooled_incompatible(tooled_incompatible),
            );
            for synonym in &model.synonyms {
                catalog.insert_synonym(synonym, alias);
            }
        }

        catalog
    }

    /// Returns the publication settings for a repository root. Overrides use
    /// canonical paths and the deepest matching ancestor wins.
    pub fn effective_publish(&self, repo_root: impl AsRef<Path>) -> PublishSettings {
        let canonical_root = fs::canonicalize(repo_root).ok();
        let override_settings = canonical_root.as_deref().and_then(|root| {
            self.publish
                .overrides
                .iter()
                .filter_map(|(path, settings)| {
                    let canonical_override = fs::canonicalize(path).ok()?;
                    root.starts_with(&canonical_override)
                        .then_some((canonical_override.components().count(), settings))
                })
                .max_by_key(|(depth, _)| *depth)
                .map(|(_, settings)| settings)
        });

        PublishSettings {
            push: override_settings
                .and_then(|settings| settings.push)
                .unwrap_or(self.publish.push),
            pr: override_settings
                .and_then(|settings| settings.pr)
                .unwrap_or(self.publish.pr),
            branches: self.publish.branches.clone(),
            remote: self.publish.remote.clone(),
            base: self.publish.base.clone(),
        }
    }
}

/// Controls how a worker's reply contract is carried over the OpenCode prompt boundary.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DispatchTransport {
    #[default]
    Text,
    Schema,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DispatchConfig {
    pub transport: DispatchTransport,
}

/// Lazily loads configuration once for the lifetime of this loader.
///
/// Construct one loader for the process rather than caching configuration on
/// disk: each invocation still observes edits made before it starts.
#[derive(Debug)]
pub struct ConfigLoader {
    state_dir: PathBuf,
    config: OnceLock<Result<OcaConfig, ConfigLoadError>>,
}

impl ConfigLoader {
    pub fn from_home(home: impl AsRef<Path>) -> Self {
        Self::from_state_dir(home.as_ref().join(STATE_DIRECTORY))
    }

    pub fn from_state_dir(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            config: OnceLock::new(),
        }
    }

    /// # Errors
    ///
    /// Returns the one cached load error when the state directory cannot be
    /// read or the config is malformed.
    pub fn get(&self) -> Result<&OcaConfig, ConfigLoadError> {
        self.config
            .get_or_init(|| {
                OcaConfig::load_from_state_dir(&self.state_dir).map_err(ConfigLoadError::from)
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    pub efforts: Vec<String>,
    pub synonyms: Vec<String>,
    pub shorthand: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RoleConfig {
    pub schema: String,
    pub preamble_file: Option<String>,
    pub permission: PermissionMode,
    pub min_words: u16,
}

impl Default for RoleConfig {
    fn default() -> Self {
        Self {
            schema: String::new(),
            preamble_file: None,
            permission: PermissionMode::Deny,
            min_words: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    Deny,
    Inherit,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct HerdrConfig {
    pub workspace: String,
    pub socket: String,
    pub timeout_ms: u64,
    pub close_on_done: bool,
}

impl Default for HerdrConfig {
    fn default() -> Self {
        Self {
            workspace: "oca".to_owned(),
            socket: String::new(),
            timeout_ms: 750,
            close_on_done: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct PublishConfig {
    pub push: bool,
    pub pr: bool,
    pub branches: String,
    pub remote: String,
    pub base: String,
    pub overrides: BTreeMap<PathBuf, PublishOverride>,
}

impl Default for PublishConfig {
    fn default() -> Self {
        Self {
            push: false,
            pr: false,
            branches: "oca/*".to_owned(),
            remote: "origin".to_owned(),
            base: String::new(),
            overrides: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PublishOverride {
    pub push: Option<bool>,
    pub pr: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishSettings {
    pub push: bool,
    pub pr: bool,
    pub branches: String,
    pub remote: String,
    pub base: String,
}

/// A non-fatal configuration key that a caller may show under `--diag`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigDiagnostic {
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    pub port: u16,
    pub alt_ports: Vec<u16>,
    pub start_timeout_ms: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 4096,
            alt_ports: vec![4097, 4098],
            start_timeout_ms: 8_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RetentionConfig {
    pub journal_days: u16,
    pub tombstone_days: u16,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            journal_days: 14,
            tombstone_days: 30,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read or write configuration: {0}")]
    Io(#[from] io::Error),
    #[error("invalid configuration TOML: {0}")]
    TomlDeserialize(#[from] toml::de::Error),
    #[error("failed to serialize configuration TOML: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
}

/// A cached error returned by [`ConfigLoader`].
#[derive(Clone, Debug, Error)]
#[error("{message}")]
pub struct ConfigLoadError {
    message: String,
}

impl From<ConfigError> for ConfigLoadError {
    fn from(error: ConfigError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

fn default_models() -> BTreeMap<String, ModelConfig> {
    DEFAULT_MODEL_DEFINITIONS
        .iter()
        .map(|definition| {
            (
                definition.alias.to_owned(),
                ModelConfig {
                    provider: definition.provider.to_owned(),
                    model: definition.model.to_owned(),
                    efforts: definition
                        .ladder
                        .iter()
                        .copied()
                        .map(ToString::to_string)
                        .collect(),
                    synonyms: definition
                        .synonyms
                        .iter()
                        .copied()
                        .map(ToString::to_string)
                        .collect(),
                    ..ModelConfig::default()
                },
            )
        })
        .collect()
}

fn default_roles() -> BTreeMap<String, RoleConfig> {
    BTreeMap::from([
        (
            "impl".to_owned(),
            RoleConfig {
                schema: "~/.oca/schemas/impl.json".to_owned(),
                preamble_file: None,
                permission: PermissionMode::Deny,
                min_words: 25,
            },
        ),
        (
            "review".to_owned(),
            RoleConfig {
                schema: "~/.oca/schemas/review.json".to_owned(),
                preamble_file: None,
                permission: PermissionMode::Deny,
                min_words: 15,
            },
        ),
    ])
}

fn merge_tables(base: &mut toml::Value, overlay: &toml::Value) {
    let (toml::Value::Table(base), toml::Value::Table(overlay)) = (base, overlay) else {
        return;
    };

    for (key, value) in overlay {
        match (base.get_mut(key), value) {
            (Some(base_value), toml::Value::Table(_)) => merge_tables(base_value, value),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

fn collect_table_diagnostics(
    document: &toml::Table,
    table_name: &str,
    known_keys: &[&str],
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    if let Some(toml::Value::Table(table)) = document.get(table_name) {
        collect_unknown_keys(table, table_name, known_keys, diagnostics);
    }
}

fn collect_unknown_keys(
    table: &toml::Table,
    path: &str,
    known_keys: &[&str],
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    for key in table
        .keys()
        .filter(|key| !known_keys.contains(&key.as_str()))
    {
        diagnostics.push(ConfigDiagnostic {
            path: format!("{path}.{key}"),
        });
    }
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_private_directory_permissions(path)
}

fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    set_private_file_creation_mode(&mut options);
    let mut file = options.open(path)?;
    set_private_file_permissions(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_creation_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_file_creation_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::OcaConfig;

    #[test]
    fn missing_config_uses_the_compiled_in_catalog_and_defaults() {
        let config = OcaConfig::from_toml_str("").expect("empty config is valid");

        assert_eq!(config.models.len(), 4);
        assert!(config.models.contains_key("luna"));
        assert!(config.models.contains_key("sol"));
        assert!(config.models.contains_key("terra"));
        assert!(config.models.contains_key("flash"));
        assert!(config.roles.contains_key("impl"));
        assert!(config.roles.contains_key("review"));
        assert!(config.roles["impl"].preamble_file.is_none());
        assert_eq!(config.dispatch.transport, super::DispatchTransport::Text);
        assert!(config.herdr.close_on_done);
        assert!(!config.publish.push);
        assert!(!config.publish.pr);
    }

    #[test]
    fn dispatch_transport_defaults_to_text_and_accepts_schema_escape_hatch() {
        let default = OcaConfig::from_toml_str("").expect("empty config is valid");
        assert_eq!(default.dispatch.transport, super::DispatchTransport::Text);

        let schema = OcaConfig::from_toml_str(
            r#"
[dispatch]
transport = "schema"
"#,
        )
        .expect("schema transport is valid");
        assert_eq!(schema.dispatch.transport, super::DispatchTransport::Schema);
    }

    #[test]
    fn most_specific_canonical_publish_override_wins() {
        let temp = tempfile::tempdir().expect("temp directory");
        let repository = temp.path().join("repository");
        let nested_repository = repository.join("nested");
        std::fs::create_dir_all(&nested_repository).expect("repository directories");

        let config = OcaConfig::from_toml_str(&format!(
            r#"
[publish]
push = false
pr = false

[publish.overrides."{}"]
push = true

[publish.overrides."{}"]
push = false
pr = true
"#,
            repository.display(),
            nested_repository.display(),
        ))
        .expect("valid overrides");

        let publish = config.effective_publish(&nested_repository);
        assert!(!publish.push);
        assert!(publish.pr);
    }

    #[test]
    fn unknown_known_table_keys_are_preserved_but_only_reported_as_diagnostics() {
        let temp = tempfile::tempdir().expect("temp directory");
        let config = OcaConfig::from_toml_str(
            r#"
[ignored_top_level]
value = "kept, but not interpreted"

[herdr]
workspace = "custom"
future_option = true
"#,
        )
        .expect("valid config with forward-compatible keys");

        assert_eq!(config.diagnostics().len(), 1);
        assert_eq!(config.diagnostics()[0].path, "herdr.future_option");

        config.write_to_home(temp.path()).expect("round-trip write");
        let written =
            std::fs::read_to_string(temp.path().join(".oca/config.toml")).expect("written config");
        assert!(written.contains("future_option = true"));
        assert!(written.contains("[ignored_top_level]"));
    }

    #[test]
    fn writes_changed_known_values_without_dropping_unknown_keys() {
        let temp = tempfile::tempdir().expect("temp directory");
        let mut config = OcaConfig::from_toml_str(
            r#"
[herdr]
workspace = "before"
future_option = true
"#,
        )
        .expect("valid config");
        config.herdr.workspace = "after".to_owned();

        config.write_to_home(temp.path()).expect("config write");
        let written =
            std::fs::read_to_string(temp.path().join(".oca/config.toml")).expect("written config");
        assert!(written.contains("workspace = \"after\""));
        assert!(written.contains("future_option = true"));
    }

    #[test]
    fn user_defined_model_tables_extend_the_catalog() {
        let config = OcaConfig::from_toml_str(
            r#"
[models.custom]
provider = "example"
model = "custom-model"
efforts = ["high"]
"#,
        )
        .expect("custom model is valid");

        assert_eq!(config.models.len(), 5);
        assert_eq!(config.models["custom"].provider, "example");
        assert_eq!(config.models["custom"].efforts, ["high"]);
    }

    #[test]
    fn model_catalog_uses_configured_models_and_synonyms() {
        let config = OcaConfig::from_toml_str(
            r#"
[models.custom]
provider = "example"
model = "custom-model"
efforts = ["high"]
synonyms = ["bespoke"]

[models.flash]
provider = "override"
model = "flash-override"
efforts = ["max"]
synonyms = []
"#,
        )
        .expect("custom models are valid");
        let catalog = config.model_catalog();

        let custom = oca_core::resolve_model("bespoke", "high", &catalog)
            .expect("configured synonym should resolve");
        assert_eq!(custom.alias, "custom");
        assert_eq!(custom.provider, "example");
        assert_eq!(custom.model, "custom-model");

        assert!(
            oca_core::resolve_model("deepseek", "max", &catalog).is_err(),
            "clearing flash synonyms in config must clear the compiled default"
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_directory_and_config_file_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp directory");
        let config = OcaConfig::load_from_home(temp.path()).expect("missing config uses defaults");
        config.write_to_home(temp.path()).expect("config write");

        let state_dir = temp.path().join(".oca");
        let config_path = state_dir.join("config.toml");
        assert_eq!(
            std::fs::metadata(state_dir)
                .expect("state dir")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(config_path)
                .expect("config file")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
