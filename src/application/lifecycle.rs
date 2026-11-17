//! Cross-command lifecycle policy: which package-database lock a command
//! needs. Lock policy is transaction policy, not argument parsing (design
//! `system-intelligence-and-migrations.md` §4.5: "Lock policy moves out of
//! CLI definitions"), so it is expressed over a typed request instead of the
//! CLI's `Cmd` enum: the CLI layer translates each parsed command into a
//! [`Request`] (see `cli::Cmd::lock_request`), and this module decides the
//! lock. Application never imports CLI types.
//!
//! Callers lock once, at the top-level command (`commands::dispatch`); the
//! library functions (`install_archive`, `upgrade`, ...) never lock, because
//! `upgrade` calls `install_archive` and flock is not re-entrant across two
//! open file descriptions. Per-step-lock commands acquire short locks
//! themselves (see the per-variant notes in `cli::Cmd::lock_request`).

use crate::lock;

/// What a command does to the installed-package database, stripped of CLI
/// argument detail. The CLI layer maps its parsed `Cmd` onto this; the
/// application layer owns what each request means for locking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    /// installing archives holds the lock for the whole command
    InstallArchives,
    /// installing by name builds first, so (like `build --install`) it locks
    /// around each install step only
    InstallByName,
    /// whole-command database writers: remove, rollback, autoremove, history
    MutateDb,
    /// upgrade: `--dry-run` only reads the database, otherwise it writes
    Upgrade { dry_run: bool },
    /// `build --install` / `update` lock themselves around the install step
    /// only (holding the db lock through a full build would starve readers)
    PerStep,
    /// read-only database inspection: `outdated` and the query family
    ReadDb,
    /// group remove takes an outer exclusive lock; group install/list/info
    /// rely on the install path's own per-transaction locks (an outer lock
    /// there would deadlock that path)
    GroupRemove,
    /// repository and sign tooling, completions, config, sandbox: commands
    /// that touch no package database at all
    NoDb,
}

/// Which database lock a command needs: writers exclusive, readers shared.
pub(crate) fn lock_mode(request: &Request) -> Option<lock::Mode> {
    use lock::Mode::{Exclusive, Shared};
    use Request::*;
    match request {
        InstallArchives | MutateDb | GroupRemove => Some(Exclusive),
        Upgrade { dry_run: true } => Some(Shared),
        Upgrade { dry_run: false } => Some(Exclusive),
        ReadDb => Some(Shared),
        InstallByName | PerStep | NoDb => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writers_take_the_exclusive_lock_and_readers_a_shared_one() {
        use Request::*;
        assert_eq!(lock_mode(&InstallArchives), Some(lock::Mode::Exclusive));
        assert_eq!(lock_mode(&MutateDb), Some(lock::Mode::Exclusive));
        assert_eq!(lock_mode(&GroupRemove), Some(lock::Mode::Exclusive));
        assert_eq!(
            lock_mode(&Upgrade { dry_run: false }),
            Some(lock::Mode::Exclusive)
        );
        assert_eq!(
            lock_mode(&Upgrade { dry_run: true }),
            Some(lock::Mode::Shared)
        );
        assert_eq!(lock_mode(&ReadDb), Some(lock::Mode::Shared));
        // per-step and non-database commands lock nothing here
        assert_eq!(lock_mode(&InstallByName), None);
        assert_eq!(lock_mode(&PerStep), None);
        assert_eq!(lock_mode(&NoDb), None);
    }
}
