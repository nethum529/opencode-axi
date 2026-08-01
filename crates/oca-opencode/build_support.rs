use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use sha2::{Digest, Sha256};

const SNAPSHOT_DIRECTORY: &str = "compatibility/opencode/1.18.10";
const FACADE_OPERATION_IDS: &[&str] = &[
    "session.create",
    "session.prompt_async",
    "session.message",
    "session.abort",
    "session.messages",
    "event.subscribe",
];

pub struct Snapshot {
    pub bytes: Vec<u8>,
    declared_sha256: String,
    source: PathBuf,
}

pub fn load_pinned_snapshot(workspace_directory: &Path) -> Result<Snapshot, String> {
    let snapshot_directory = workspace_directory.join(SNAPSHOT_DIRECTORY);
    let source = snapshot_directory.join("openapi.json");
    let metadata_path = snapshot_directory.join("snapshot.toml");
    let bytes = fs::read(&source).map_err(|error| {
        format!(
            "could not read pinned OpenCode snapshot {}: {error}",
            source.display()
        )
    })?;
    let metadata = fs::read_to_string(&metadata_path).map_err(|error| {
        format!(
            "could not read pinned OpenCode snapshot metadata {}: {error}",
            metadata_path.display()
        )
    })?;
    let metadata: toml::Value = toml::from_str(&metadata).map_err(|error| {
        format!(
            "could not parse pinned OpenCode snapshot metadata {}: {error}",
            metadata_path.display()
        )
    })?;
    let declared_sha256 = metadata
        .get("sha256")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| {
            format!(
                "pinned OpenCode snapshot metadata {} has no sha256",
                metadata_path.display()
            )
        })?
        .to_owned();

    Ok(Snapshot {
        bytes,
        declared_sha256,
        source,
    })
}

pub fn verify_snapshot_hash(snapshot: &Snapshot) -> Result<(), String> {
    let actual_sha256 = format!("{:x}", Sha256::digest(&snapshot.bytes));
    if actual_sha256 == snapshot.declared_sha256 {
        Ok(())
    } else {
        Err(format!(
            "OpenCode snapshot SHA-256 mismatch at {}: snapshot.toml declares {}, but openapi.json is {}",
            snapshot.source.display(),
            snapshot.declared_sha256,
            actual_sha256
        ))
    }
}

pub fn generate_client(snapshot: &Snapshot) -> Result<String, String> {
    let mut document: Value = serde_json::from_slice(&snapshot.bytes)
        .map_err(|error| format!("could not parse pinned OpenCode OpenAPI snapshot: {error}"))?;
    retain_facade_operations(&mut document);
    retain_success_responses(&mut document);
    normalize_openapi_31(&mut document);
    preserve_additional_properties(&mut document);
    let specification: openapiv3::OpenAPI = serde_json::from_value(document)
        .map_err(|error| format!("could not decode pinned OpenCode OpenAPI snapshot: {error}"))?;
    let tokens = progenitor::Generator::default()
        .generate_tokens(&specification)
        .map_err(|error| {
            format!("could not generate OpenCode client from pinned snapshot: {error}")
        })?;
    let syntax = syn::parse2(tokens)
        .map_err(|error| format!("could not parse generated OpenCode client: {error}"))?;

    Ok(prettyplease::unparse(&syntax))
}

fn retain_success_responses(value: &mut Value) {
    let Value::Object(object) = value else {
        if let Value::Array(values) = value {
            for value in values {
                retain_success_responses(value);
            }
        }
        return;
    };

    if let Some(responses) = object.get_mut("responses").and_then(Value::as_object_mut) {
        responses.retain(|status, _| status.starts_with('2'));
    }
    for value in object.values_mut() {
        retain_success_responses(value);
    }
}

fn retain_facade_operations(document: &mut Value) {
    let Some(paths) = document.get_mut("paths").and_then(Value::as_object_mut) else {
        return;
    };
    paths.retain(|_, path_item| {
        let Some(path_item) = path_item.as_object_mut() else {
            return false;
        };
        path_item.retain(|method, operation| {
            method == "parameters"
                || operation
                    .get("operationId")
                    .and_then(Value::as_str)
                    .is_some_and(|operation_id| FACADE_OPERATION_IDS.contains(&operation_id))
        });
        path_item.keys().any(|key| key != "parameters")
    });
}

fn normalize_openapi_31(value: &mut Value) {
    let Value::Object(object) = value else {
        if let Value::Array(values) = value {
            for value in values {
                normalize_openapi_31(value);
            }
        }
        return;
    };

    if object.get("openapi").and_then(Value::as_str) == Some("3.1.0") {
        object.insert("openapi".to_owned(), Value::String("3.0.3".to_owned()));
    }
    normalize_null_unions(object, "anyOf");
    normalize_null_unions(object, "oneOf");
    normalize_parameter_style(object);
    normalize_exclusive_bound(object, "exclusiveMinimum", "minimum");
    normalize_exclusive_bound(object, "exclusiveMaximum", "maximum");
    for value in object.values_mut() {
        normalize_openapi_31(value);
    }
}

fn normalize_parameter_style(object: &mut serde_json::Map<String, Value>) {
    if object.get("style").and_then(Value::as_str) == Some("deepObject") {
        object.insert("style".to_owned(), Value::String("form".to_owned()));
    }
}

fn normalize_null_unions(object: &mut serde_json::Map<String, Value>, keyword: &str) {
    let Some(Value::Array(variants)) = object.get_mut(keyword) else {
        return;
    };
    let has_null_variant = variants.iter().any(is_null_schema);
    if has_null_variant {
        variants.retain(|variant| !is_null_schema(variant));
        object.insert("nullable".to_owned(), Value::Bool(true));
    }
}

fn is_null_schema(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.get("type") == Some(&Value::String("null".to_owned())))
}

fn normalize_exclusive_bound(
    object: &mut serde_json::Map<String, Value>,
    exclusive_key: &str,
    inclusive_key: &str,
) {
    let Some(bound) = object.get(exclusive_key).cloned().filter(Value::is_number) else {
        return;
    };
    object.entry(inclusive_key.to_owned()).or_insert(bound);
    object.insert(exclusive_key.to_owned(), Value::Bool(true));
}

fn preserve_additional_properties(value: &mut Value) {
    let Value::Object(object) = value else {
        if let Value::Array(values) = value {
            for value in values {
                preserve_additional_properties(value);
            }
        }
        return;
    };

    for value in object.values_mut() {
        preserve_additional_properties(value);
    }

    let has_properties = object
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| !properties.is_empty());
    let permits_untyped_additional_properties = !object.contains_key("additionalProperties")
        || object.get("additionalProperties") == Some(&Value::Bool(true));
    if object.get("type") == Some(&Value::String("object".to_owned()))
        && has_properties
        && permits_untyped_additional_properties
    {
        object.insert(
            "additionalProperties".to_owned(),
            Value::Object(serde_json::Map::default()),
        );
    }
}

pub fn generated_file(out_directory: &Path) -> PathBuf {
    out_directory.join("opencode_client.rs")
}

pub fn write_generated_file(out_directory: &Path, generated: &str) -> Result<PathBuf, String> {
    let destination = generated_file(out_directory);
    fs::write(&destination, generated).map_err(|error| {
        format!(
            "could not write generated OpenCode client {}: {error}",
            destination.display()
        )
    })?;
    Ok(destination)
}
