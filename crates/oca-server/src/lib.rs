//! Discovery and lifecycle management for the local `opencode serve` process.

mod readiness;
mod server;

pub use readiness::ServerHealth;
pub use server::{
    ConnectError, ConnectOrStart, OpenCodeRequest, RequestFailure, ServerRecord, ServerRuntime,
    StartupDiagnostic, StartupStage, SystemRuntime, default_start_environment_hash,
};
