//! Typed, durable access to the state that belongs to `oca`.

mod config;
mod refs;

pub use config::{
    ConfigDiagnostic, ConfigError, ConfigLoadError, ConfigLoader, HerdrConfig, ModelConfig,
    OcaConfig, PermissionMode, PublishConfig, PublishOverride, PublishSettings, RetentionConfig,
    RoleConfig, ServerConfig,
};
pub use refs::{
    AtomicWriteHook, NewRef, PendingRefAllocation, RefAllocationCompletion, RefDurabilityWarning,
    RefIdSource, RefListFilter, RefPatch, RefRecord, RefStore, RefStoreError, RefStorePaths,
};
