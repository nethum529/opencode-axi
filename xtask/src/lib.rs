use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

const DEFAULT_OPENCODE_URL: &str = "http://127.0.0.1:4096";
const PINNED_OPENCODE_VERSION: &str = "1.18.10";
const OPENAPI_VERSION: &str = "1.0.0";

#[derive(Debug, PartialEq, Eq)]
struct RefreshCommand {
    url: String,
    version: String,
}

fn parse_command<I, S>(arguments: I) -> Result<RefreshCommand, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut arguments = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_owned());
    match arguments.next().as_deref() {
        Some("refresh-opencode-spec") => {}
        Some(command) => return Err(format!("unknown xtask command {command:?}")),
        None => return Err("missing xtask command".to_owned()),
    }

    let mut url = None;
    let mut version = None;
    while let Some(argument) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| format!("missing value for {argument}"))?;
        match argument.as_str() {
            "--url" if url.is_none() => url = Some(value),
            "--version" if version.is_none() => version = Some(value),
            "--url" | "--version" => return Err(format!("duplicate option {argument}")),
            _ => return Err(format!("unknown refresh-opencode-spec option {argument}")),
        }
    }

    Ok(RefreshCommand {
        url: url.unwrap_or_else(|| DEFAULT_OPENCODE_URL.to_owned()),
        version: version.unwrap_or_else(|| PINNED_OPENCODE_VERSION.to_owned()),
    })
}

/// Executes `refresh-opencode-spec` from the workspace root.
///
/// # Errors
///
/// Returns an error when command arguments are invalid, `/doc` cannot be fetched, or the fetched
/// document cannot be turned into a verified compatibility snapshot.
pub fn run(arguments: impl IntoIterator<Item = String>) -> Result<(), String> {
    let command = parse_command(arguments)?;
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "xtask manifest has no workspace parent".to_owned())?;
    refresh(&command.url, &command.version, workspace_root)
}

fn refresh(url: &str, version: &str, workspace_root: &Path) -> Result<(), String> {
    let endpoint = format!("{}/doc", url.trim_end_matches('/'));
    let response = ureq::get(&endpoint)
        .call()
        .map_err(|error| format!("could not fetch {endpoint}: {error}"))?;
    let document = response
        .into_body()
        .read_to_vec()
        .map_err(|error| format!("could not read {endpoint}: {error}"))?;
    let output_directory = workspace_root
        .join("compatibility")
        .join("opencode")
        .join(version);
    refresh_from_document(&document, version, &output_directory)
}

fn canonicalize_json(document: &[u8]) -> Result<Vec<u8>, String> {
    let value = serde_json::from_slice(document).map_err(|error| error.to_string())?;
    let canonical = sort_json(value);
    let mut bytes = serde_json::to_vec_pretty(&canonical).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn verify_declared_version(document: &[u8], expected: &str) -> Result<(), String> {
    let document: Value = serde_json::from_slice(document).map_err(|error| error.to_string())?;
    let found = document
        .pointer("/info/version")
        .and_then(Value::as_str)
        .ok_or_else(|| "OpenCode /doc is missing info.version".to_owned())?;

    if found == expected {
        Ok(())
    } else {
        Err(format!(
            "OpenCode /doc version mismatch: expected {expected}, found {found}"
        ))
    }
}

const SURFACE_OPERATION_IDS: [&str; 6] = [
    "session.create",
    "session.prompt_async",
    "session.message",
    "session.abort",
    "session.messages",
    "event.subscribe",
];
const HTTP_METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Surface {
    operations: Vec<SurfaceOperation>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct SurfaceOperation {
    operation_id: String,
    method: String,
    path: String,
}

fn resolve_surface(document: &[u8]) -> Result<Surface, String> {
    let document: Value = serde_json::from_slice(document).map_err(|error| error.to_string())?;
    let paths = document
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| "OpenCode /doc is missing paths".to_owned())?;

    let mut operations = Vec::with_capacity(SURFACE_OPERATION_IDS.len());
    for operation_id in SURFACE_OPERATION_IDS {
        let found = paths.iter().find_map(|(path, path_item)| {
            let path_item = path_item.as_object()?;
            HTTP_METHODS.iter().find_map(|method| {
                let operation = path_item.get(*method)?.as_object()?;
                (operation.get("operationId").and_then(Value::as_str) == Some(operation_id)).then(
                    || SurfaceOperation {
                        operation_id: operation_id.to_owned(),
                        method: method.to_ascii_uppercase(),
                        path: path.clone(),
                    },
                )
            })
        });
        operations.push(found.ok_or_else(|| {
            format!("OpenCode /doc is missing required operationId {operation_id}")
        })?);
    }

    Ok(Surface { operations })
}

fn refresh_from_document(
    document: &[u8],
    version: &str,
    output_directory: &Path,
) -> Result<(), String> {
    validate_version(version)?;
    let canonical = canonicalize_json(document)?;
    verify_declared_version(&canonical, OPENAPI_VERSION)?;
    let surface = resolve_surface(&canonical)?;
    let document: Value = serde_json::from_slice(&canonical).map_err(|error| error.to_string())?;
    let digest = sha256_hex(&canonical);
    let metadata = format!(
        "version = \"{version}\"\nopenapi_version = \"{OPENAPI_VERSION}\"\nsha256 = \"{digest}\"\ngenerator = \"cargo xtask refresh-opencode-spec\"\nendpoint_count = {}\n",
        endpoint_count(&document)?
    );
    let mut surface = serde_json::to_vec_pretty(&surface).map_err(|error| error.to_string())?;
    surface.push(b'\n');

    fs::create_dir_all(output_directory).map_err(|error| {
        format!(
            "could not create snapshot directory {}: {error}",
            output_directory.display()
        )
    })?;
    write_artifact(&output_directory.join("openapi.json"), &canonical)?;
    write_artifact(&output_directory.join("snapshot.toml"), metadata.as_bytes())?;
    write_artifact(&output_directory.join("surface.json"), &surface)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn validate_version(version: &str) -> Result<(), String> {
    if !version.is_empty()
        && version
            .bytes()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, b'.' | b'-'))
    {
        Ok(())
    } else {
        Err(format!("invalid OpenCode version {version:?}"))
    }
}

fn endpoint_count(document: &Value) -> Result<usize, String> {
    let paths = document
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| "OpenCode /doc is missing paths".to_owned())?;
    Ok(paths
        .values()
        .filter_map(Value::as_object)
        .map(|path_item| {
            path_item
                .keys()
                .filter(|method| HTTP_METHODS.contains(&method.as_str()))
                .count()
        })
        .sum())
}

fn write_artifact(path: &Path, contents: &[u8]) -> Result<(), String> {
    fs::write(path, contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn sort_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(sort_json).collect()),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.into_iter().collect();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, sort_json(value)))
                    .collect::<Map<_, _>>(),
            )
        }
        primitive => primitive,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_OPENCODE_URL, PINNED_OPENCODE_VERSION, RefreshCommand, canonicalize_json,
        parse_command, refresh_from_document, resolve_surface, sha256_hex, verify_declared_version,
    };
    use std::{
        env, fs, process,
        time::{SystemTime, UNIX_EPOCH},
    };

    // Seams under test: canonicalize_json, verify_declared_version, resolve_surface,
    // and write_refresh_artifacts. Each test adds one vertical refresh slice.
    #[test]
    fn canonicalize_json_sorts_keys_at_every_depth() {
        let document = br#"{"z":[{"b":2,"a":1}],"a":true}"#;

        let canonical = canonicalize_json(document).expect("synthetic fixture is valid JSON");

        assert_eq!(
            String::from_utf8(canonical).expect("canonical JSON is UTF-8"),
            concat!(
                "{\n",
                "  \"a\": true,\n",
                "  \"z\": [\n",
                "    {\n",
                "      \"a\": 1,\n",
                "      \"b\": 2\n",
                "    }\n",
                "  ]\n",
                "}\n"
            )
        );
    }

    #[test]
    fn verify_declared_version_rejects_a_different_openapi_version() {
        let document = br#"{"info":{"version":"1.18.10"},"paths":{}}"#;

        let error = verify_declared_version(document, "1.18.8")
            .expect_err("a response from another OpenCode release must not be snapshotted");

        assert_eq!(
            error,
            "OpenCode /doc version mismatch: expected 1.18.8, found 1.18.10"
        );
    }

    #[test]
    fn resolve_surface_uses_operation_ids_to_record_actual_paths_and_methods() {
        let document = br#"
        {
          "paths": {
            "/session": {"post": {"operationId": "session.create"}},
            "/session/{sessionID}/abort": {"post": {"operationId": "session.abort"}},
            "/session/{sessionID}/message": {"get": {"operationId": "session.messages"}},
            "/session/{sessionID}/prompt_async": {"post": {"operationId": "session.prompt_async"}},
            "/session/{sessionID}/message/{messageID}": {"get": {"operationId": "session.message"}},
            "/event": {"get": {"operationId": "event.subscribe"}}
          }
        }
        "#;

        let surface = resolve_surface(document).expect("all required operation IDs are present");

        assert_eq!(
            surface
                .operations
                .iter()
                .map(|operation| {
                    (
                        operation.operation_id.as_str(),
                        operation.method.as_str(),
                        operation.path.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                ("session.create", "POST", "/session"),
                (
                    "session.prompt_async",
                    "POST",
                    "/session/{sessionID}/prompt_async"
                ),
                (
                    "session.message",
                    "GET",
                    "/session/{sessionID}/message/{messageID}"
                ),
                ("session.abort", "POST", "/session/{sessionID}/abort"),
                ("session.messages", "GET", "/session/{sessionID}/message"),
                ("event.subscribe", "GET", "/event"),
            ]
        );
    }

    #[test]
    fn refresh_writes_a_digest_for_the_freshly_canonicalized_snapshot() {
        let document = br#"
        {
          "paths": {
            "/session": {"post": {"operationId": "session.create"}},
            "/session/{sessionID}/abort": {"post": {"operationId": "session.abort"}},
            "/session/{sessionID}/message": {"get": {"operationId": "session.messages"}},
            "/session/{sessionID}/prompt_async": {"post": {"operationId": "session.prompt_async"}},
            "/session/{sessionID}/message/{messageID}": {"get": {"operationId": "session.message"}},
            "/event": {"get": {"operationId": "event.subscribe"}}
          },
          "info": {"version": "1.0.0"}
        }
        "#;
        let output_root = env::temp_dir().join(format!(
            "oca-xtask-test-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time is after the Unix epoch")
                .as_nanos()
        ));

        refresh_from_document(document, "1.18.8", &output_root)
            .expect("synthetic OpenAPI document refreshes");

        let snapshot = fs::read(output_root.join("openapi.json")).expect("snapshot is written");
        let metadata =
            fs::read_to_string(output_root.join("snapshot.toml")).expect("metadata is written");
        assert_eq!(
            snapshot,
            canonicalize_json(document).expect("fixture is valid JSON")
        );
        assert!(metadata.contains(&format!("sha256 = \"{}\"", sha256_hex(&snapshot))));
        assert!(metadata.contains("endpoint_count = 6"));
        assert!(output_root.join("surface.json").is_file());

        fs::remove_dir_all(output_root).expect("test output can be removed");
    }

    #[test]
    fn parse_command_accepts_the_refresh_contract_flags_in_any_order() {
        assert_eq!(
            parse_command([
                "refresh-opencode-spec",
                "--version",
                "1.18.8",
                "--url",
                "http://127.0.0.1:4096",
            ]),
            Ok(RefreshCommand {
                url: "http://127.0.0.1:4096".to_owned(),
                version: "1.18.8".to_owned(),
            })
        );
    }

    #[test]
    fn parse_command_uses_the_pinned_local_server_by_default() {
        assert_eq!(
            parse_command(["refresh-opencode-spec"]),
            Ok(RefreshCommand {
                url: DEFAULT_OPENCODE_URL.to_owned(),
                version: PINNED_OPENCODE_VERSION.to_owned(),
            })
        );
    }
}
