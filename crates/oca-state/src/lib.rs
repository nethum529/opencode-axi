//! Typed, durable access to the state that belongs to `oca`.

mod config;
mod intents;
mod journal;
mod refs;
mod session;

pub use config::{
    ConfigDiagnostic, ConfigError, ConfigLoadError, ConfigLoader, HerdrConfig, ModelConfig,
    OcaConfig, PermissionMode, PublishConfig, PublishOverride, PublishSettings, RetentionConfig,
    RoleConfig, ServerConfig,
};
pub use intents::{
    INTENT_SCHEMA_VERSION, Intent, IntentOperation, IntentPhase, IntentRequest, IntentStore,
    IntentStoreError,
};
pub use journal::{
    EventJournal, JournalError, JournalEvent, JournalPage, MAX_JOURNAL_RECORD_BYTES,
    prune_expired_journals,
};
pub use refs::{
    AtomicWriteHook, NewRef, PendingRefAllocation, RefAllocationCompletion, RefDurabilityWarning,
    RefIdSource, RefListFilter, RefPatch, RefRecord, RefStore, RefStoreError, RefStorePaths,
};
pub use session::{RefState, SessionTurnLock};
