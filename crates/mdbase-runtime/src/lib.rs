//! Durable, provider-neutral execution for mdbase Runtime profile 0.2.
//!
//! Core mdbase owns contract identity and record projection; the interoperability
//! profile owns CloudEvents and action/source/provider declarations. This crate
//! consumes that verified evidence and owns admission, durable runs, recovery,
//! and one-shot timers. Embedding hosts remain the final authorization boundary.

mod activity;
mod admission;
mod clock;
mod engine;
mod error;
mod memory;
mod model;
mod planner;
#[cfg(feature = "postgres")]
mod postgres;
mod provider;
mod schemas;
mod store;
mod timer;

#[cfg(feature = "sqlite")]
mod sqlite;

pub use activity::{watch_event_envelope, StatusTransitionActivity};
pub use clock::{Clock, ManualClock, SystemClock};
pub use engine::{DeliveryOutcome, Runtime, RuntimeBuilder, RuntimeConfig, WorkerOutcome};
pub use error::{RuntimeError, RuntimeResult};
pub use mdbase_interop::{
    ActionCancellation, ActionInvocation, ActionOutcome, ExactContractReference,
    ImplementationIdentity, PortableError,
};
pub use memory::InMemoryRuntimeStore;
pub use model::{
    ActionAttempt, ActionAttemptStatus, ActionDispatch, CancellationOutcome, ConcurrencyPolicy,
    DispatchFailure, DispatchOutcome, EventJournalEntry, OnError, PlannedRun, PlannedStep,
    RunRecord, RunStatus, RuntimeFailure, StepRecord, StepStatus, TimerRecord, TimerStatus,
};
#[cfg(feature = "postgres")]
pub use postgres::{PostgresRuntimeStore, POSTGRES_SCHEMA_VERSION};
pub use provider::{
    ActionProvider, AuthorizationDecision, DenyAllAuthorizer, DispatchAuthorizer, ProviderBinding,
    ProviderRegistry,
};
pub use schemas::validate_runtime_record;
#[cfg(feature = "sqlite")]
pub use sqlite::{
    inspect_sqlite_recovery, SqliteRecoveryState, SqliteRuntimeStore, SQLITE_SCHEMA_VERSION,
};
pub use store::{
    AdmitOutcome, Claim, EventPage, PreparedEvent, RuntimeStore, StoreSnapshot, TimerClaim,
    TimerReconcileOutcome,
};
pub use timer::{TimerFireOutcome, TimerReconcileRequest, TimerRequest, TIMER_GENERATION_MAX};

/// Independently versioned implementation package.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Runtime profile implemented by this crate.
pub const PROFILE_VERSION: &str = "0.2";
pub use admission::{canonical_digest, AdmissionCatalog};
