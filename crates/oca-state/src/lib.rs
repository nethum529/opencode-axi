//! Typed, durable access to the state that belongs to `oca`.

mod config;
mod journal;
mod refs;

pub use config::{
    ConfigDiagnostic, ConfigError, ConfigLoadError, ConfigLoader, HerdrConfig, ModelConfig,
    OcaConfig, PermissionMode, PublishConfig, PublishOverride, PublishSettings, RetentionConfig,
    RoleConfig, ServerConfig,
};
pub use journal::{EventJournal, JournalError, MAX_JOURNAL_RECORD_BYTES};
pub use refs::{
    AtomicWriteHook, NewRef, PendingRefAllocation, RefAllocationCompletion, RefDurabilityWarning,
    RefIdSource, RefListFilter, RefPatch, RefRecord, RefStore, RefStoreError, RefStorePaths,
};
