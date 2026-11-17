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
//! * this layer may depend on the CLI types it routes, the domain, and
//!   its own ports;
//! * it must never depend on adapter implementations directly — external
//!   effects go through ports (introduced as the refactor progresses).

pub(crate) mod lifecycle;
