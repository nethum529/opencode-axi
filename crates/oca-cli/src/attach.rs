//! Detached headed-display lifecycle for one dispatched worker.

use std::{path::Path, time::Duration};

use oca_core::{
    DisplayMode, FollowBoundaryOutcome, FollowTarget, RefId, follow_until_terminal_boundary,
};
use oca_display::{HerdrClient, TmuxClient, opencode_attach_argv};
use oca_opencode::OpenCodeClient;
use oca_server::ConnectOrStart;
use oca_state::{EventJournal, OcaConfig, RefPatch, RefStore, RefStorePaths};
use url::Url;

use crate::{
    AttachCommand,
    attach_diagnostics::{
        HistoryProbe, herdr_agent_name, journal_history_diagnostic, probe_history, worker_identity,
    },
};

/// Runs the hidden, detached headed attach helper.
///
/// Display is an isolated best-effort subsystem. Any configuration, herdr,
/// state, or follow failure ends only this helper and cannot retroactively
/// fail the dispatch that already acknowledged the worker.
pub async fn execute_attach(command: &AttachCommand, home: impl AsRef<Path>) {
    if let Err(error) = run_attach(command, home.as_ref()).await {
        eprintln!("warning: headed display attach failed: {error}");
    }
}

async fn run_attach(command: &AttachCommand, home: &Path) -> Result<(), String> {
    let config = OcaConfig::load_from_home(home)
        .map_err(|error| format!("could not load configuration: {error}"))?;
    let state_directory = home.join(".oca");
    let refs = RefStore::with_paths(RefStorePaths::in_directory(&state_directory));
    let record = refs
        .resolve(&command.reference)
        .map_err(|error| format!("could not resolve ref: {error}"))?
        .ok_or_else(|| format!("ref `{}` no longer exists", command.reference))?;
    if record.session_id != command.session_id {
        return Err(format!(
            "ref `{}` belongs to session `{}`, not `{}`",
            command.reference, record.session_id, command.session_id
        ));
    }
    let message_id = record
        .message_id
        .clone()
        .ok_or_else(|| format!("ref `{}` has no attributed message id", command.reference))?;
    let directory = record
        .session_directory()
        .ok_or_else(|| {
            format!(
                "ref `{}` has no stored session directory",
                command.reference
            )
        })?
        .to_owned();
    let reference = RefId::new(&record.id)
        .map_err(|error| format!("ref `{}` is invalid: {error}", command.reference))?;
    let mode = match record.display.as_deref() {
        Some("herdr") => DisplayMode::Herdr,
        Some("tmux") => DisplayMode::Tmux,
        Some("headless") | None => return Ok(()),
        Some(mode) => {
            return Err(format!(
                "ref `{}` has unknown display mode `{mode}`",
                command.reference
            ));
        }
    };
    let server = ConnectOrStart::from_home(home, &config.server)
        .read_record()
        .map_err(|error| format!("could not read OpenCode server record: {error}"))?
        .ok_or_else(|| "no OpenCode server record is available".to_owned())?;
    let base_url = Url::parse(&format!("http://127.0.0.1:{}", server.port))
        .map_err(|error| format!("invalid OpenCode server URL: {error}"))?;
    let client = OpenCodeClient::new(base_url.clone());
    let history = probe_history(&client, &command.session_id).await;
    let identity = worker_identity(&command.reference, &record, &config, &history);
    let agent_name = herdr_agent_name(&command.reference, &record, &config);

    match mode {
        DisplayMode::Herdr => {
            let turn = AttachTurn {
                state_directory: &state_directory,
                reference: &reference,
                base_url: &base_url,
                message_id: &message_id,
                directory: &directory,
                worker_identity: &identity,
                agent_name: &agent_name,
                client: &client,
                history: &history,
            };
            run_herdr_attach(command, home, &config, &refs, turn).await
        }
        DisplayMode::Tmux => {
            let turn = AttachTurn {
                state_directory: &state_directory,
                reference: &reference,
                base_url: &base_url,
                message_id: &message_id,
                directory: &directory,
                worker_identity: &identity,
                agent_name: &agent_name,
                client: &client,
                history: &history,
            };
            run_tmux_attach(command, turn).await
        }
        DisplayMode::Headless => Ok(()),
    }
}

struct AttachTurn<'a> {
    state_directory: &'a Path,
    reference: &'a RefId,
    base_url: &'a Url,
    message_id: &'a str,
    directory: &'a str,
    worker_identity: &'a str,
    agent_name: &'a str,
    client: &'a OpenCodeClient,
    history: &'a HistoryProbe,
}

async fn run_herdr_attach(
    command: &AttachCommand,
    home: &Path,
    config: &OcaConfig,
    refs: &RefStore,
    turn: AttachTurn<'_>,
) -> Result<(), String> {
    let configured_socket =
        (!config.herdr.socket.is_empty()).then(|| Path::new(config.herdr.socket.as_str()));
    let Some(herdr) = HerdrClient::discover_from(
        home,
        configured_socket,
        Duration::from_millis(config.herdr.timeout_ms),
    ) else {
        return Ok(());
    };

    let workspace = herdr
        .workspace(&config.herdr.workspace)
        .await
        .map_err(|error| error.to_string())?;
    let tab = herdr
        .tab(&workspace, &command.display_name, true, &command.cwd)
        .await
        .map_err(|error| error.to_string())?;

    let launch_result = async {
        herdr
            .agent_start(
                &tab,
                turn.agent_name,
                opencode_attach_argv(turn.base_url.as_str(), &command.session_id),
            )
            .await
            .map_err(|error| error.to_string())?;
        refs.patch(
            &command.reference,
            RefPatch::default().with_herdr_tab(tab.as_str()),
        )
        .map_err(|error| format!("could not record herdr tab: {error}"))
    }
    .await;
    if let Err(error) = launch_result {
        // Before the tab id is durable, `oca k` cannot clean it up. A launch
        // or persistence failure therefore retains best-effort local cleanup.
        let _ = herdr.close_tab(&tab).await;
        return Err(error);
    }

    let follow_result = async {
        let mut journal =
            EventJournal::create(turn.state_directory, turn.reference, turn.message_id)
                .map_err(|error| format!("could not create event journal: {error}"))?;
        journal_history_diagnostic(&mut journal, &command.session_id, turn.history)
            .map_err(|error| format!("could not record history diagnostic: {error}"))?;
        let target = FollowTarget {
            session_id: command.session_id.clone(),
            message_id: turn.message_id.to_owned(),
            directory: turn.directory.to_owned(),
        };
        follow_until_terminal_boundary::<_, EventJournal>(
            turn.client,
            &target,
            None,
            Some(&mut journal),
        )
        .await
        .map_err(|error| format!("could not follow worker terminal state: {error}"))
    }
    .await;

    match follow_result {
        Ok(FollowBoundaryOutcome::Terminal) if config.herdr.close_on_done => herdr
            .close_tab(&tab)
            .await
            .map_err(|error| error.to_string()),
        Ok(_) => Ok(()),
        // The persisted tab remains explicitly killable when following fails;
        // absence of a terminal boundary must never close a live worker tab.
        Err(error) => Err(error),
    }
}

async fn run_tmux_attach(command: &AttachCommand, turn: AttachTurn<'_>) -> Result<(), String> {
    let tmux = TmuxClient::default();
    let window = tmux
        .new_window(
            &command.reference,
            &command.display_name,
            turn.worker_identity,
            turn.base_url.as_str(),
            &command.session_id,
            &command.cwd,
        )
        .map_err(|error| error.to_string())?;
    let follow_result = async {
        let mut journal =
            EventJournal::create(turn.state_directory, turn.reference, turn.message_id)
                .map_err(|error| format!("could not create event journal: {error}"))?;
        journal_history_diagnostic(&mut journal, &command.session_id, turn.history)
            .map_err(|error| format!("could not record history diagnostic: {error}"))?;
        let target = FollowTarget {
            session_id: command.session_id.clone(),
            message_id: turn.message_id.to_owned(),
            directory: turn.directory.to_owned(),
        };
        follow_until_terminal_boundary::<_, EventJournal>(
            turn.client,
            &target,
            None,
            Some(&mut journal),
        )
        .await
        .map_err(|error| format!("could not follow worker terminal state: {error}"))
    }
    .await;

    match follow_result {
        Ok(FollowBoundaryOutcome::Terminal) => tmux
            .close_window(&window)
            .map_err(|error| error.to_string()),
        Ok(_) => Ok(()),
        Err(error) => {
            // A window created before follow failure would otherwise be
            // orphaned. Cleanup remains best effort.
            let _ = tmux.close_window(&window);
            Err(error)
        }
    }
}
