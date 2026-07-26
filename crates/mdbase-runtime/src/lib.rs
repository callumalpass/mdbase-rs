//! Durable, provider-neutral execution for mdbase Runtime profile 0.1.
//!
//! The collection crate owns contract and CEL semantics. This crate owns event
//! admission, workflow planning, durable runs, action dispatch, recovery, and
//! one-shot timers. Embedding hosts remain the final authorization boundary.

mod activity;
mod clock;
mod engine;
mod error;
mod memory;
mod model;
mod planner;
#[cfg(feature = "postgres")]
mod postgres;
mod provider;
mod store;
mod timer;

#[cfg(feature = "sqlite")]
mod sqlite;

pub use activity::{watch_event_envelope, StatusTransitionActivity};
pub use clock::{Clock, ManualClock, SystemClock};
pub use engine::{DeliveryOutcome, Runtime, RuntimeBuilder, RuntimeConfig, WorkerOutcome};
pub use error::{RuntimeError, RuntimeResult};
pub use memory::InMemoryRuntimeStore;
pub use model::{
    ActionAttempt, ActionAttemptStatus, ActionDispatch, ActionResponse, CancellationOutcome,
    ConcurrencyPolicy, DispatchFailure, DispatchOutcome, EventJournalEntry, OnError, PlannedRun,
    PlannedStep, RunRecord, RunStatus, RuntimeFailure, StepRecord, StepStatus, TimerRecord,
    TimerStatus,
};
#[cfg(feature = "postgres")]
pub use postgres::{PostgresRuntimeStore, POSTGRES_SCHEMA_VERSION};
pub use provider::{
    ActionProvider, AuthorizationDecision, DenyAllAuthorizer, DispatchAuthorizer, ProviderRegistry,
};
#[cfg(feature = "sqlite")]
pub use sqlite::{SqliteRuntimeStore, SQLITE_SCHEMA_VERSION};
pub use store::{
    AdmitOutcome, Claim, EventPage, PreparedEvent, RuntimeStore, StoreSnapshot, TimerClaim,
    TimerReconcileOutcome,
};
pub use timer::{TimerFireOutcome, TimerReconcileRequest, TimerRequest};

/// Independently versioned implementation package.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Runtime profile implemented by this crate.
pub const PROFILE_VERSION: &str = mdbase::runtime_contracts::RUNTIME_PROFILE_VERSION;
