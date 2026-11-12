//! Plugin system (roadmap Phase 2).
//!
//! Versioned, capability-limited extensions OUTSIDE the core process: JSON
//! Lines over stdin/stdout, one-shot invocations, bounded output, timeouts.
//! A plugin crash or a malformed message can never crash Nawa, and plugin
//! output is untrusted input everywhere.
//!
//! Discovery: `/usr/lib/nawa/plugins` (system), `/usr/local/lib/nawa/plugins`
//! (local admin) and `$NAWA_PLUGIN_PATH` entries. A root nawa never executes
//! a plugin discovered in a user's home directory.

pub(crate) mod discovery;
pub(crate) mod events;
pub(crate) mod manifest;
pub(crate) mod protocol;
pub(crate) mod runner;
pub(crate) mod sandbox;
