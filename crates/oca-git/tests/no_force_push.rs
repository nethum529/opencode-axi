//! Publication must remain structurally incapable of force pushing.

use std::{fs, path::Path};

#[test]
fn production_rust_has_no_force_push_argument_or_forced_refspec() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate is nested under the workspace");
    let mut sources = Vec::new();
    collect_rust_sources(&workspace.join("crates"), &mut sources);
    let long_option = ["--", "force"].concat();
    let short_option = ["\"-", "f\""].concat();
    let forced_refspec = ["+", "refs"].concat();

    for path in sources {
        let source = fs::read_to_string(&path).expect("Rust source is readable");
        let production = source.split("#[cfg(test)]").next().unwrap_or(&source);
        assert!(
            !production.contains(&long_option),
            "{} contains a force-push option",
            path.display()
        );
        assert!(
            !production.contains(&forced_refspec),
            "{} contains a forced refspec",
            path.display()
        );
        for push in production.match_indices("\"push\"") {
            let start = push.0.saturating_sub(256);
            let end = production.len().min(push.0 + 512);
            assert!(
                !production[start..end].contains(&short_option),
                "{} can place the short force option in a push invocation",
                path.display()
            );
        }
    }
}

fn collect_rust_sources(directory: &Path, sources: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(directory).expect("source directory is readable") {
        let path = entry.expect("source entry is readable").path();
        if path.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) != Some("tests") {
                collect_rust_sources(&path, sources);
            }
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            sources.push(path);
        }
    }
}
