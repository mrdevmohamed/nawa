//! The build sandbox — docs/design/build-isolation.md §4.
//!
//! `nawa sandbox-init <spec.toml>` is spawned by the build engine instead of
//! `bash` when isolation is `sandbox` or `closure`. It:
//!
//!   1. `unshare(CLONE_NEWUSER)`; the *parent* writes a ranged uid/gid map
//!      when it runs as real root (fake root, §4.3); otherwise the process
//!      self-writes a single own-uid map (the unprivileged pattern);
//!   2. `unshare(CLONE_NEWNS|NEWPID|NEWIPC|NEWUTS[|NEWNET])`;
//!   3. forks the real build process as PID 1 of the new PID namespace —
//!      when it exits, the kernel kills every straggler;
//!   4. PID 1 builds the sandbox filesystem, drops all capabilities, sets
//!      `NO_NEW_PRIVS`, `RLIMIT_CORE=0`, umask 022 and execs
//!      `bash -e -c <script>` with the whitelisted environment only.
//!
//! The spec is a plain TOML file (the `toml` crate is already a dependency —
//! no `serde_json`, so the offline vendored bootstrap of nawa itself keeps
//! working unchanged). Everything that *decides* the sandbox stays in
//! ordinary testable Rust (`spec.rs`); the mount recipes live in
//! `mounts.rs`; process/namespace orchestration in `init.rs`; kernel
//! capability checks in `probe.rs`; the confined `unsafe` helpers for what
//! rustix does not cover are in [`crate::sys`].
//!
//! This stops accidents and impurity (wrong-but-linkable libraries, builds
//! that write outside `DESTDIR`, leftover daemons, wall-clock timestamps),
//! not a determined attacker — sources are sha256-pinned and recipes are
//! trusted code (design §2 non-goals).

mod init;
pub mod mounts;
mod probe;
pub mod spec;

use crate::sys;

pub use init::{first_untraversable_ancestor, init, parent_sync, spawn_init};
pub use probe::{probe, PROBES};
pub use spec::{BuildPaths, RootSpec, SandboxSpec};

/// Exit code convention: 124 = timed out (like timeout(1)).
pub const EXIT_TIMEOUT: i32 = 124;

/// Exit code PID 1 returns when its exec must not have returned.
pub(crate) const EXEC_MUST_NOT_RETURN: i32 = 126;

/// Exit code the parent reports for a PID 1 setup failure.
pub(crate) const PID1_SETUP_FAILED: i32 = 125;
