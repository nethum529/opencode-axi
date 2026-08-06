//! Detached headed-display lifecycle for one dispatched worker.

use std::{path::Path, time::Duration};

use oca_core::{DisplayMode, FollowOutcome, FollowTarget, follow_until_terminal};
use oca_display::{HerdrClient, TmuxClient};
use oca_opencode::OpenCodeClient;
use oca_server::ConnectOrStart;
use oca_state::{EventJournal, OcaConfig, RefPatch, RefStore, RefStorePaths};
use url::Url;

use crate::AttachCommand;

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
        .ok_or_else(|| format!("ref `{}` has no attributed message id", command.reference))?;
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

    match mode {
        DisplayMode::Herdr => {
            run_herdr_attach(command, home, &config, &refs, &base_url, message_id).await
        }
        DisplayMode::Tmux => run_tmux_attach(command, &base_url, message_id).await,
        DisplayMode::Headless => Ok(()),
    }
}

async fn run_herdr_attach(
    command: &AttachCommand,
    home: &Path,
    config: &OcaConfig,
    refs: &RefStore,
    base_url: &Url,
    message_id: String,
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
        .tab(&workspace, &command.reference, true, &command.cwd)
        .await
        .map_err(|error| error.to_string())?;

    let attach_result = async {
        herdr
            .agent_start(
                &tab,
                vec![
                    "opencode".to_owned(),
                    "attach".to_owned(),
                    base_url.to_string(),
                    "--session".to_owned(),
                    command.session_id.clone(),
                ],
            )
            .await
            .map_err(|error| error.to_string())?;
        refs.patch(
            &command.reference,
            RefPatch::default().with_herdr_tab(tab.as_str()),
        )
        .map_err(|error| format!("could not record herdr tab: {error}"))?;

        let target = FollowTarget {
            session_id: command.session_id.clone(),
            message_id,
        };
        follow_until_terminal::<_, EventJournal>(
            &OpenCodeClient::new(base_url.clone()),
            &target,
            None,
            None,
        )
        .await
        .map_err(|error| format!("could not follow worker terminal state: {error}"))
    }
    .await;

    match attach_result {
        Ok(FollowOutcome::Terminal(_)) if config.herdr.close_on_done => herdr
            .close_tab(&tab)
            .await
            .map_err(|error| error.to_string()),
        Ok(_) => Ok(()),
        Err(error) => {
            // A tab created before a launch or persistence failure would
            // otherwise be orphaned. Cleanup remains best effort.
            let _ = herdr.close_tab(&tab).await;
            Err(error)
        }
    }
}

async fn run_tmux_attach(
    command: &AttachCommand,
    base_url: &Url,
    message_id: String,
) -> Result<(), String> {
    let tmux = TmuxClient::default();
    let window = tmux
        .new_window(&command.reference, &command.session_id, &command.cwd)
        .map_err(|error| error.to_string())?;
    let target = FollowTarget {
        session_id: command.session_id.clone(),
        message_id,
    };
    let follow_result = follow_until_terminal::<_, EventJournal>(
        &OpenCodeClient::new(base_url.clone()),
        &target,
        None,
        None,
    )
    .await
    .map_err(|error| format!("could not follow worker terminal state: {error}"));

    match follow_result {
        Ok(FollowOutcome::Terminal(_)) => tmux
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
