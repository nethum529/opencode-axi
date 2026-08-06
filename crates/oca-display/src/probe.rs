//! Bounded liveness probe for a candidate herdr socket path.

use std::{path::Path, time::Duration};

use socket2::{Domain, SockAddr, Socket, Type};

/// Deadline for the discovery connect attempt.
///
/// A refused or absent socket fails the connect immediately, so this budget is
/// only spent when the kernel makes the connect wait, such as a listener whose
/// accept backlog is full. It is deliberately far shorter than
/// `HerdrClient::DEFAULT_TIMEOUT`, which bounds a whole protocol request.
const DISCOVERY_CONNECT_TIMEOUT: Duration = Duration::from_millis(100);

/// Reports whether `path` has a listener willing to accept a connection.
///
/// A socket file left behind by an uncleanly killed herdr refuses the connect,
/// so it reads as absent and display selection falls through to tmux or
/// headless. The probe opens and immediately drops a connection without
/// writing a protocol request, and it never unlinks the path: discovery must
/// stay side-effect free, and a path that momentarily refuses may belong to a
/// herdr that is restarting.
pub(crate) fn socket_accepts_connections(path: &Path) -> bool {
    let Ok(address) = SockAddr::unix(path) else {
        return false;
    };
    let Ok(socket) = Socket::new(Domain::UNIX, Type::STREAM, None) else {
        return false;
    };
    socket
        .connect_timeout(&address, DISCOVERY_CONNECT_TIMEOUT)
        .is_ok()
}
