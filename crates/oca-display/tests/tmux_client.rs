#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    sync::{Mutex, MutexGuard, PoisonError},
};

use oca_core::FollowBoundaryTerminal;
use oca_display::{TmuxClient, TmuxError};

static TMUX_FIXTURE_LOCK: Mutex<()> = Mutex::new(());
const WORKER_IDENTITY: &str =
    "wabc12 | impl's agent | openai/gpt-5.6 | high | DO NOT TYPE: composer unbound";

#[test]
fn creates_and_closes_only_the_ref_owned_window_against_a_fake_tmux() {
    let fixture = Fixture::new(0);
    let client = TmuxClient::new(fixture.executable.as_os_str());

    let window = client
        .new_window(
            "wabc12",
            "fixParser",
            WORKER_IDENTITY,
            "http://127.0.0.1:4096/",
            "ses_target",
            &fixture.cwd,
        )
        .unwrap();
    assert_eq!(window.name(), "fixParser");
    assert_eq!(window.id(), "@42");
    client
        .mark_terminal(&window, WORKER_IDENTITY, FollowBoundaryTerminal::Done)
        .unwrap();
    client.close_window(&window).unwrap();
    assert_eq!(
        fixture.calls(),
        [
            format!("cwd={}", fixture.cwd.display()),
            "new-window".to_owned(),
            "-d".to_owned(),
            "-P".to_owned(),
            "-F".to_owned(),
            "#{window_id}".to_owned(),
            "-n".to_owned(),
            "fixParser".to_owned(),
            "--".to_owned(),
            "opencode".to_owned(),
            "attach".to_owned(),
            "http://127.0.0.1:4096/".to_owned(),
            "--session".to_owned(),
            "ses_target".to_owned(),
            "--call--".to_owned(),
            "set-option".to_owned(),
            "-w".to_owned(),
            "-t".to_owned(),
            "@42".to_owned(),
            "@oca-ref".to_owned(),
            "wabc12".to_owned(),
            "--call--".to_owned(),
            "set-option".to_owned(),
            "-w".to_owned(),
            "-t".to_owned(),
            "@42".to_owned(),
            "@oca-identity".to_owned(),
            WORKER_IDENTITY.to_owned(),
            "--call--".to_owned(),
            "set-option".to_owned(),
            "-w".to_owned(),
            "-t".to_owned(),
            "@42".to_owned(),
            "pane-border-status".to_owned(),
            "top".to_owned(),
            "--call--".to_owned(),
            "set-option".to_owned(),
            "-w".to_owned(),
            "-t".to_owned(),
            "@42".to_owned(),
            "pane-border-format".to_owned(),
            "#{@oca-identity}".to_owned(),
            "--call--".to_owned(),
            "set-option".to_owned(),
            "-w".to_owned(),
            "-t".to_owned(),
            "@42".to_owned(),
            "@oca-identity".to_owned(),
            format!("{WORKER_IDENTITY} | DONE"),
            "--call--".to_owned(),
            "kill-window".to_owned(),
            "-t".to_owned(),
            "@42".to_owned(),
            "--call--".to_owned(),
        ]
    );
}

#[test]
fn resets_the_identity_to_failed_for_an_errored_terminal_boundary() {
    let fixture = Fixture::new(0);
    let client = TmuxClient::new(fixture.executable.as_os_str());
    let window = client
        .new_window(
            "wabc12",
            "fixParser",
            WORKER_IDENTITY,
            "http://127.0.0.1:4096/",
            "ses_target",
            &fixture.cwd,
        )
        .unwrap();

    client
        .mark_terminal(&window, WORKER_IDENTITY, FollowBoundaryTerminal::Failed)
        .unwrap();

    let calls = fixture.calls();
    assert_eq!(
        &calls[calls.len() - 7..],
        [
            "set-option",
            "-w",
            "-t",
            "@42",
            "@oca-identity",
            &format!("{WORKER_IDENTITY} | FAILED"),
            "--call--",
        ]
    );
}

#[test]
fn resets_the_identity_for_each_reply_backed_non_done_marker() {
    let fixture = Fixture::new(0);
    let client = TmuxClient::new(fixture.executable.as_os_str());
    let window = client
        .new_window(
            "wabc12",
            "fixParser",
            WORKER_IDENTITY,
            "http://127.0.0.1:4096/",
            "ses_target",
            &fixture.cwd,
        )
        .unwrap();

    for (terminal, expected) in [
        (FollowBoundaryTerminal::Partial, "PARTIAL"),
        (FollowBoundaryTerminal::Blocked, "BLOCKED"),
        (FollowBoundaryTerminal::Unclear, "UNCLEAR"),
    ] {
        client
            .mark_terminal(&window, WORKER_IDENTITY, terminal)
            .unwrap();
        let calls = fixture.calls();
        assert_eq!(
            calls[calls.len() - 2],
            format!("{WORKER_IDENTITY} | {expected}")
        );
    }
}

#[test]
fn a_fake_tmux_failure_is_typed() {
    let fixture = Fixture::new(7);
    let error = TmuxClient::new(fixture.executable.as_os_str())
        .new_window(
            "wabc12",
            "fixParser",
            WORKER_IDENTITY,
            "http://127.0.0.1:4096/",
            "ses_target",
            &fixture.cwd,
        )
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

#[test]
fn a_window_id_tmux_never_prints_is_typed() {
    let fixture = Fixture::with_window_id(0, "window one");
    let error = TmuxClient::new(fixture.executable.as_os_str())
        .new_window(
            "wabc12",
            "fixParser",
            WORKER_IDENTITY,
            "http://127.0.0.1:4096/",
            "ses_target",
            &fixture.cwd,
        )
        .unwrap_err();

    assert!(
        matches!(&error, TmuxError::InvalidWindowId { output } if output == "window one"),
        "{error}"
    );
    assert!(
        !fixture.calls().iter().any(|call| call == "kill-window"),
        "an unusable window id leaves nothing to close: {:?}",
        fixture.calls()
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
        Self::with_window_id(exit_code, "@42")
    }

    fn with_window_id(exit_code: u8, window_id: &str) -> Self {
        // A sibling test can otherwise fork while this fixture is writing its
        // script. The child inherits the write descriptor and Linux can reject
        // the script exec with ETXTBSY (errno 26).
        // A failing sibling test poisons the lock; take it anyway so the
        // failure reports once instead of twice.
        let lock = TMUX_FIXTURE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("tmux");
        let cwd = temp.path().join("worker");
        let log = temp.path().join("calls");
        fs::create_dir(&cwd).unwrap();
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nif [ \"$1\" = new-window ]; then printf 'cwd=%s\\n' \"$PWD\" >> '{}'; printf '{window_id}\\n'; fi\nprintf '%s\\n' \"$@\" >> '{}'\nprintf '%s\\n' --call-- >> '{}'\nexit {exit_code}\n",
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
