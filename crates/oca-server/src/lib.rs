//! Discovery and lifecycle management for the local `opencode serve` process.

mod server;

pub use server::{
    ConnectError, ConnectOrStart, OpenCodeRequest, RequestFailure, ServerRecord, ServerRuntime,
    StartupDiagnostic, StartupStage, SystemRuntime, default_start_environment_hash,
};
