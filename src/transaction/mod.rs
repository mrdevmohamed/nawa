//! Transaction history (roadmap Phase 1).
//!
//! A *transaction* is one logical user command (`nawa install firefox`); a
//! *package change* is one step inside it. Transactions live as immutable
//! TOML records under `<root>/var/lib/nawa/history/transactions/`, with a
//! write-ahead step journal (`journal/<id>/steps/`) so a crash between the
//! package-database commit and the history write can be reconciled later.
//!
//! Locking: ID allocation and every history mutation happen while the caller
//! holds the exclusive database lock (the same `<root>/var/lib/nawa/lock`
//! the installer uses) — no second lock, no lock-ordering cycle. Builds and
//! downloads stay outside the lock, exactly as before.

pub(crate) mod context;
pub(crate) mod history;
pub(crate) mod journal;
pub(crate) mod outbox;
pub(crate) mod recovery;

// The full module surface is re-exported for the CLI layers; individual
// items may be consumed only by later roadmap phases.
#[allow(unused_imports)]
pub(crate) use context::{begin_operation, no_history, OperationCommand, OperationHandle, RecorderHandle};
#[allow(unused_imports)]
pub(crate) use history::{
    PackageChange, PackageState, StepAction, StepStatus, TransactionRecord, TxStatus,
    SCHEMA_VERSION,
};
