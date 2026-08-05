//! Cross-crate contract for the compiled-in model definitions.
//!
//! Runtime values prove the catalog and empty configuration agree. The source
//! assertion below additionally prevents a future copied literal from merely
//! happening to have equal values.

use std::path::Path;

use oca_core::{DEFAULT_MODEL_DEFINITIONS, ModelCatalog, resolve_model};
use oca_state::OcaConfig;

#[test]
fn default_model_definition_drives_catalog_and_configuration() {
    let catalog = ModelCatalog::default();
    let config = OcaConfig::from_toml_str("").expect("an empty config is valid");

    assert_eq!(
        catalog.aliases().collect::<Vec<_>>(),
        ["flash", "luna", "sol", "terra"]
    );
    assert_eq!(config.models.len(), DEFAULT_MODEL_DEFINITIONS.len());

    for definition in DEFAULT_MODEL_DEFINITIONS {
        let catalog_model = catalog
            .get(definition.alias)
            .unwrap_or_else(|| panic!("{} is in the default catalog", definition.alias));
        let config_model = config
            .models
            .get(definition.alias)
            .unwrap_or_else(|| panic!("{} is in the default config", definition.alias));

        assert_eq!(catalog_model.provider, definition.provider);
        assert_eq!(catalog_model.model, definition.model);
        assert_eq!(catalog_model.ladder, definition.ladder);
        assert_eq!(config_model.provider, definition.provider);
        assert_eq!(config_model.model, definition.model);
        assert_eq!(config_model.efforts, definition.ladder);
        assert_eq!(config_model.synonyms, definition.synonyms);

        for synonym in definition.synonyms {
            let direct = resolve_model(definition.alias, "high", &catalog)
                .expect("the canonical alias resolves");
            let through_synonym =
                resolve_model(synonym, "high", &catalog).expect("the synonym resolves");
            assert_eq!(through_synonym, direct, "{synonym} resolves as the alias");
        }
    }
}

/// The source between two markers. A missing marker panics rather than
/// widening the slice, so renaming either boundary fails loudly here.
fn constructor_body<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let (_, after_start) = source
        .split_once(start)
        .unwrap_or_else(|| panic!("`{start}` opens a default constructor"));
    let (body, _) = after_start
        .split_once(end)
        .unwrap_or_else(|| panic!("`{end}` closes a default constructor"));
    body
}

#[test]
fn default_constructors_cannot_use_duplicated_model_tables() {
    let state_source = include_str!("../src/config.rs");
    let core_source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../oca-core/src/resolver.rs"),
    )
    .expect("oca-core resolver source is readable");

    let catalog_constructor = constructor_body(
        &core_source,
        "impl Default for ModelCatalog",
        "/// The two CLI effort sources",
    );
    let config_constructor =
        constructor_body(state_source, "fn default_models()", "fn default_roles()");

    for constructor in [catalog_constructor, config_constructor] {
        assert!(
            constructor.contains("DEFAULT_MODEL_DEFINITIONS"),
            "default constructors must derive from the shared model definition, not a copied table"
        );
        for copied_literal in [
            "gpt-5.6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "deepseek-v4-flash-free",
        ] {
            assert!(
                !constructor.contains(copied_literal),
                "{copied_literal} must appear only in DEFAULT_MODEL_DEFINITIONS"
            );
        }
    }
}
