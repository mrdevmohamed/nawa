//! Application layer: use-case orchestration and cross-command lifecycle
//! coordination.
//!
//! The application layer sits between the CLI and the domain. It owns
//! operation coordination that no single command handler should own:
//! which database lock a command needs (see [`lifecycle`]), and later the
//! recovery/reconciliation and post-transaction sequencing that
//! `main.rs` currently performs inline.
//!
//! Dependency rules (enforced by `scripts/check-layering.py`):
//! * the CLI layer translates its parsed commands into typed requests (see
//!   `cli::Cmd::lock_request`); this layer owns the policy, never CLI types;
//! * use cases decide and render nothing — results and notices are returned
//!   for the command handlers to print (the presentation interaction port
//!   lands with the apply-transaction extraction);
//! * adapter implementations stay behind ports as the refactor progresses
//!   (until then use cases may call adapter modules, but never presentation
//!   code: `cli`, `commands`, `output`, `color`, `diag`, `humanize`,
//!   `confirm`, `bootstrap`).

pub(crate) mod features;
pub(crate) mod lifecycle;
