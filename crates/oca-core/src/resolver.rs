use std::{borrow::Borrow, collections::BTreeMap, fmt};

use crate::error::{ErrorCode, OcaError};

/// The canonical effort names understood by the resolver.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Effort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "l" | "low" => Some(Self::Low),
            "m" | "medium" => Some(Self::Medium),
            "h" | "high" => Some(Self::High),
            "x" | "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A model alias after it has been normalized for catalog lookup.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Alias(String);

impl Alias {
    pub fn new(value: impl AsRef<str>) -> Self {
        Self(value.as_ref().trim().to_ascii_lowercase())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Alias {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for Alias {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for Alias {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// One configured model and the effort ladder it supports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelDefinition {
    pub provider: String,
    pub model: String,
    pub ladder: Vec<String>,
}

pub type ModelEntry = ModelDefinition;
pub type ModelSpec = ModelDefinition;

impl ModelDefinition {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        ladder: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            ladder: ladder.into_iter().map(Into::into).collect(),
        }
    }

    pub fn ladder(&self) -> impl Iterator<Item = &str> {
        self.ladder.iter().map(String::as_str)
    }
}

/// One entry in the model catalog compiled into a first-run installation.
///
/// This is the single source for the resolver catalog and the state
/// configuration defaults. Consumers should derive their representations from
/// [`DEFAULT_MODEL_DEFINITIONS`] rather than copying this table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DefaultModelDefinition {
    pub alias: &'static str,
    pub provider: &'static str,
    pub model: &'static str,
    pub ladder: &'static [&'static str],
    pub synonyms: &'static [&'static str],
}

/// The model aliases available before a user supplies configuration.
pub const DEFAULT_MODEL_DEFINITIONS: &[DefaultModelDefinition] = &[
    DefaultModelDefinition {
        alias: "luna",
        provider: "openai",
        model: "gpt-5.6-luna",
        ladder: &["low", "medium", "high", "xhigh", "max"],
        synonyms: &[],
    },
    DefaultModelDefinition {
        alias: "sol",
        provider: "openai",
        model: "gpt-5.6-sol",
        ladder: &["low", "medium", "high", "xhigh", "max"],
        synonyms: &[],
    },
    DefaultModelDefinition {
        alias: "terra",
        provider: "openai",
        model: "gpt-5.6-terra",
        ladder: &["low", "medium", "high", "xhigh", "max"],
        synonyms: &[],
    },
    DefaultModelDefinition {
        alias: "flash",
        provider: "opencode",
        model: "deepseek-v4-flash-free",
        ladder: &["high", "max"],
        synonyms: &["deepseek"],
    },
];

impl From<&DefaultModelDefinition> for ModelDefinition {
    fn from(definition: &DefaultModelDefinition) -> Self {
        Self::new(
            definition.provider,
            definition.model,
            definition.ladder.iter().copied(),
        )
    }
}

/// The alias-to-model catalog used by the pure resolver.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCatalog {
    pub models: BTreeMap<String, ModelDefinition>,
    synonyms: BTreeMap<String, String>,
}

pub type Catalog = ModelCatalog;

impl ModelCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        alias: impl Into<String>,
        definition: impl Into<ModelDefinition>,
    ) -> Option<ModelDefinition> {
        self.models
            .insert(Alias::new(alias.into()).0, definition.into())
    }

    pub fn get(&self, alias: impl AsRef<str>) -> Option<&ModelDefinition> {
        self.models.get(Alias::new(alias).as_str())
    }

    pub fn aliases(&self) -> impl Iterator<Item = &str> {
        self.models.keys().map(String::as_str)
    }

    fn canonical_alias(&self, alias: Alias) -> Alias {
        self.synonyms.get(alias.as_str()).map_or(alias, Alias::new)
    }
}

impl Default for ModelCatalog {
    fn default() -> Self {
        let models = DEFAULT_MODEL_DEFINITIONS
            .iter()
            .map(|definition| (definition.alias.to_owned(), definition.into()))
            .collect();
        let synonyms = DEFAULT_MODEL_DEFINITIONS
            .iter()
            .flat_map(|definition| {
                definition
                    .synonyms
                    .iter()
                    .map(move |synonym| ((*synonym).to_owned(), definition.alias.to_owned()))
            })
            .collect();

        Self { models, synonyms }
    }
}

/// The two CLI effort sources. Equal values from both sources are accepted once;
/// different values are rejected before ladder support is checked.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EffortInput {
    pub inline: Option<String>,
    pub flag: Option<String>,
}

impl EffortInput {
    pub fn one(value: impl Into<String>) -> Self {
        Self {
            inline: Some(value.into()),
            flag: None,
        }
    }

    pub fn both<I, F>(inline: Option<I>, flag: Option<F>) -> Self
    where
        I: Into<String>,
        F: Into<String>,
    {
        Self {
            inline: inline.map(Into::into),
            flag: flag.map(Into::into),
        }
    }
}

impl From<&str> for EffortInput {
    fn from(value: &str) -> Self {
        Self::one(value)
    }
}

impl From<String> for EffortInput {
    fn from(value: String) -> Self {
        Self::one(value)
    }
}

impl<'a> From<Option<&'a str>> for EffortInput {
    fn from(value: Option<&'a str>) -> Self {
        Self {
            inline: value.map(str::to_owned),
            flag: None,
        }
    }
}

impl From<Option<String>> for EffortInput {
    fn from(value: Option<String>) -> Self {
        Self {
            inline: value,
            flag: None,
        }
    }
}

impl From<&Option<String>> for EffortInput {
    fn from(value: &Option<String>) -> Self {
        Self {
            inline: value.clone(),
            flag: None,
        }
    }
}

impl<'a> From<(Option<&'a str>, Option<&'a str>)> for EffortInput {
    fn from((inline, flag): (Option<&'a str>, Option<&'a str>)) -> Self {
        Self::both(inline, flag)
    }
}

impl From<(Option<String>, Option<String>)> for EffortInput {
    fn from((inline, flag): (Option<String>, Option<String>)) -> Self {
        Self::both(inline, flag)
    }
}

/// The fully resolved provider/model/variant tuple.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedModel {
    pub alias: String,
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub variant: String,
}

/// Resolve an alias and its effort without opening a socket or consulting any
/// external state.
///
/// # Errors
///
/// Returns an error when the alias is not in the catalog, no effort is
/// supplied, the two effort sources disagree, or the requested effort cannot
/// be represented by the alias's ladder.
pub fn resolve_model<A, E, C>(alias: A, effort: E, catalog: C) -> Result<ResolvedModel, OcaError>
where
    A: AsRef<str>,
    E: Into<EffortInput>,
    C: Borrow<ModelCatalog>,
{
    let requested_alias = Alias::new(alias);
    let catalog = catalog.borrow();
    let canonical_alias = catalog.canonical_alias(requested_alias);

    // Alias validation intentionally precedes every effort validation.
    let definition = catalog.get(canonical_alias.as_str()).ok_or_else(|| {
        OcaError::new(ErrorCode::AliasUnknown)
            .with_error(format!(
                "unknown model alias `{}`",
                canonical_alias.as_str()
            ))
            .with_help("choose one of the configured model aliases")
    })?;

    let effort = effort.into();
    let requested_effort = select_effort(&effort)?;
    let variant = resolve_variant(requested_effort, &definition.ladder).ok_or_else(|| {
        let ladder = definition.ladder.join(", ");
        OcaError::new(ErrorCode::EffortUnsupported)
            .with_error(format!(
                "effort `{requested_effort}` is unsupported for `{}`; available ladder: {ladder}",
                canonical_alias.as_str()
            ))
            .with_help(format!("use one of the available efforts: {ladder}"))
    })?;

    Ok(ResolvedModel {
        alias: canonical_alias.0,
        provider: definition.provider.clone(),
        model: definition.model.clone(),
        effort: requested_effort.to_string(),
        variant,
    })
}

fn select_effort(input: &EffortInput) -> Result<Effort, OcaError> {
    let (Some(inline), Some(flag)) = (&input.inline, &input.flag) else {
        let Some(value) = input.inline.as_deref().or(input.flag.as_deref()) else {
            return Err(OcaError::new(ErrorCode::EffortMissing)
                .with_error("model effort is required")
                .with_help("provide an effort after `:` or with `-e`"));
        };
        return Effort::parse(value).ok_or_else(|| unsupported_effort(value));
    };

    if normalize_effort(inline) != normalize_effort(flag) {
        return Err(OcaError::new(ErrorCode::EffortConflict)
            .with_error(format!("conflicting efforts `{inline}` and `{flag}`"))
            .with_help("provide one effort, or make `:effort` and `-e effort` agree"));
    }

    Effort::parse(inline).ok_or_else(|| unsupported_effort(inline))
}

fn normalize_effort(value: &str) -> String {
    match Effort::parse(value) {
        Some(effort) => effort.to_string(),
        None => value.trim().to_ascii_lowercase(),
    }
}

fn unsupported_effort(value: &str) -> OcaError {
    OcaError::new(ErrorCode::EffortUnsupported)
        .with_error(format!("unsupported effort `{}`", value.trim()))
        .with_help("use one of low, medium, high, xhigh, or max")
}

fn resolve_variant(requested: Effort, ladder: &[String]) -> Option<String> {
    let normalized = ladder
        .iter()
        .filter_map(|rung| Effort::parse(rung).map(|effort| (effort, rung)))
        .collect::<Vec<_>>();

    if normalized.is_empty() {
        return None;
    }

    if let Some((_, variant)) = normalized.iter().find(|(effort, _)| *effort == requested) {
        return Some((*variant).clone());
    }

    // Requests at the two highest levels are fulfilled by the ladder's top
    // rung when that exact level is absent. This is what lets `flash:x`
    // resolve to `max` despite having no `xhigh` rung, while a low request on
    // flash remains an unsupported effort.
    let top = normalized.last().expect("checked non-empty");
    (requested >= Effort::XHigh).then(|| top.1.clone())
}
