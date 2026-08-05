//! Derivation of the local spawner and repository scopes used by dispatch and list.

use std::{
    env, fs,
    fs::OpenOptions,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const SPAWNER_FILE: &str = "spawner";
const HARNESS_SESSION_VARIABLES: &[&str] = &["OPENCODE_SESSION_ID", "CODEX_THREAD_ID"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Scope {
    pub(crate) spawner_tag: String,
    pub(crate) repo: String,
}

pub(crate) fn current(home: &Path, cwd: &Path) -> io::Result<Scope> {
    Ok(Scope {
        spawner_tag: spawner_tag(home)?,
        repo: repository_root(cwd)?.display().to_string(),
    })
}

fn spawner_tag(home: &Path) -> io::Result<String> {
    if let Some(value) = nonempty_environment("OCA_SPAWNER") {
        return Ok(value);
    }
    if let Some(value) = HARNESS_SESSION_VARIABLES
        .iter()
        .find_map(|name| nonempty_environment(name))
    {
        return Ok(value);
    }

    let state_directory = home.join(".oca");
    fs::create_dir_all(&state_directory)?;
    let path = state_directory.join(SPAWNER_FILE);
    match fs::read_to_string(&path) {
        Ok(value) if !value.trim().is_empty() => return Ok(value.trim().to_owned()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut random = [0_u8; 2];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let tag = format!("oca-{:02x}{:02x}", random[0], random[1]);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(tag.as_bytes())?;
            file.write_all(b"\n")?;
            file.flush()?;
            Ok(tag)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut file = OpenOptions::new().read(true).open(path)?;
            let mut value = String::new();
            file.read_to_string(&mut value)?;
            let value = value.trim();
            if value.is_empty() {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the persisted spawner tag is empty",
                ))
            } else {
                Ok(value.to_owned())
            }
        }
        Err(error) => Err(error),
    }
}

fn nonempty_environment(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn repository_root(cwd: &Path) -> io::Result<PathBuf> {
    let cwd = fs::canonicalize(cwd)?;
    for candidate in cwd.ancestors() {
        if candidate.join(".git").exists() {
            return Ok(candidate.to_path_buf());
        }
    }
    Ok(cwd)
}

#[cfg(test)]
mod tests {
    use super::repository_root;

    #[test]
    fn repository_scope_walks_up_from_nested_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        let nested = root.path().join("one/two");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(repository_root(&nested).unwrap(), root.path());
    }
}
