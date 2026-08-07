#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    sync::{Mutex, MutexGuard},
};

use oca_display::{TmuxClient, TmuxError};

static TMUX_FIXTURE_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn creates_and_closes_only_the_ref_owned_window_against_a_fake_tmux() {
    let fixture = Fixture::new(0);
    let client = TmuxClient::new(fixture.executable.as_os_str());

    let window = client
        .new_window("wabc12", "ses_target", &fixture.cwd)
        .unwrap();
    client.close_window(&window).unwrap();

    assert_eq!(window.name(), "oca-wabc12");
    assert_eq!(
        fixture.calls(),
        [
            format!("cwd={}", fixture.cwd.display()),
            "new-window".to_owned(),
            "-d".to_owned(),
            "-n".to_owned(),
            "oca-wabc12".to_owned(),
            "--".to_owned(),
            "opencode".to_owned(),
            "--session".to_owned(),
            "ses_target".to_owned(),
            "--call--".to_owned(),
            "kill-window".to_owned(),
            "-t".to_owned(),
            "=oca-wabc12".to_owned(),
            "--call--".to_owned(),
        ]
    );
}

#[test]
fn a_fake_tmux_failure_is_typed() {
    let fixture = Fixture::new(7);
    let error = TmuxClient::new(fixture.executable.as_os_str())
        .new_window("wabc12", "ses_target", &fixture.cwd)
        .unwrap_err();

    assert!(
        matches!(
            error,
            TmuxError::CommandFailed {
                operation: "new-window",
                ..
            }
        ),
        "{error}"
    );
}

struct Fixture {
    _lock: MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    executable: std::path::PathBuf,
    cwd: std::path::PathBuf,
    log: std::path::PathBuf,
}

impl Fixture {
    fn new(exit_code: u8) -> Self {
        // A sibling test can otherwise fork while this fixture is writing its
        // script. The child inherits the write descriptor and Linux can reject
        // the script exec with ETXTBSY (errno 26).
        let lock = TMUX_FIXTURE_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("tmux");
        let cwd = temp.path().join("worker");
        let log = temp.path().join("calls");
        fs::create_dir(&cwd).unwrap();
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nif [ \"$1\" = new-window ]; then printf 'cwd=%s\\n' \"$PWD\" >> '{}'; fi\nprintf '%s\\n' \"$@\" >> '{}'\nprintf '%s\\n' --call-- >> '{}'\nexit {exit_code}\n",
                log.display(),
                log.display(),
                log.display(),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).unwrap();
        Self {
            _lock: lock,
            _temp: temp,
            executable,
            cwd,
            log,
        }
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }
}
