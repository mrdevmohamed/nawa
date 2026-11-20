//! Cross-command lifecycle policy: which package-database lock a command
//! needs. Lock policy is transaction policy, not argument parsing (design
//! `system-intelligence-and-migrations.md` §4.5: "Lock policy moves out of
//! CLI definitions"), so it is expressed over a typed request instead of the
//! CLI's `Cmd` enum: the CLI layer translates each parsed command into a
//! [`Request`] (see `cli::Cmd::lock_request`), and this module decides the
//! lock. Application never imports CLI types — and never the lock
//! mechanism either: it states its need in its own [`LockNeed`] vocabulary,
//! and the imperative shell maps that onto the flock mechanism
//! (`lock::Mode`), keeping the dependency direction application ← shell.
//!
//! Callers lock once, at the top-level command (`commands::dispatch`); the
//! library functions (`install_archive`, `upgrade`, ...) never lock, because
//! `upgrade` calls `install_archive` and flock is not re-entrant across two
//! open file descriptions. Per-step-lock commands acquire short locks
//! themselves (see the per-variant notes in `cli::Cmd::lock_request`).

/// How a command must hold the package database while it runs: writers
/// need to exclude everyone, readers only need to exclude writers. The
/// application's own vocabulary — the shell translates it to the
/// mechanism's `lock::Mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockNeed {
    /// exclude all other nawa processes for the whole command
    Exclusive,
    /// exclude writers only; readers may proceed concurrently
    Shared,
}

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
    /// read-only inspection under a shared lock: `outdated`, the query
    /// family, and the `system migrate` record inspection (all read nawa
    /// state under `<root>/var/lib/nawa`; readers exclude writers only)
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
pub(crate) fn lock_need(request: &Request) -> Option<LockNeed> {
    use LockNeed::{Exclusive, Shared};
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
        assert_eq!(lock_need(&InstallArchives), Some(LockNeed::Exclusive));
        assert_eq!(lock_need(&MutateDb), Some(LockNeed::Exclusive));
        assert_eq!(lock_need(&GroupRemove), Some(LockNeed::Exclusive));
        assert_eq!(
            lock_need(&Upgrade { dry_run: false }),
            Some(LockNeed::Exclusive)
        );
        assert_eq!(
            lock_need(&Upgrade { dry_run: true }),
            Some(LockNeed::Shared)
        );
        assert_eq!(lock_need(&ReadDb), Some(LockNeed::Shared));
        // per-step and non-database commands lock nothing here
        assert_eq!(lock_need(&InstallByName), None);
        assert_eq!(lock_need(&PerStep), None);
        assert_eq!(lock_need(&NoDb), None);
    }
}
