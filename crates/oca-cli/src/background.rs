//! Production wiring for background dispatch ownership.

use std::path::Path;

use oca_core::{OcaError, run_background};

use crate::{
    DispatchCommand,
    foreground::{PostAckDurability, prepare_dispatch},
};

/// Executes one parsed background dispatch and returns after acknowledgement
/// and detached attach admission. Terminal SSE waiting and finalization belong
/// to a separately invoked `oca f` process.
///
/// # Errors
///
/// Returns a stable oca error for local resolution, server discovery, session
/// creation, subscription, prompt admission, ref persistence, or output.
pub async fn execute_background(
    command: DispatchCommand,
    home: impl AsRef<Path>,
) -> Result<(), OcaError> {
    let mut prepared = prepare_dispatch(command, home, PostAckDurability::Transfer)?;
    run_background(&mut prepared.backend, prepared.request)
        .await
        .map(|_| ())
}
