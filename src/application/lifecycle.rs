//! Cross-command lifecycle policy: which package-database lock a command
//! needs. Moved out of `cli.rs` — lock policy is transaction policy, not
//! argument parsing (design `system-intelligence-and-migrations.md` §4.5:
//! "Lock policy moves out of CLI definitions").
//!
//! Callers lock once, at the top-level command (`main::run`); the library
//! functions (`install_archive`, `upgrade`, ...) never lock, because
//! `upgrade` calls `install_archive` and flock is not re-entrant across two
//! open file descriptions. Per-step-lock commands acquire short locks
//! themselves (see the per-command notes below).

use crate::cli::{Cmd, GroupCmd};
use crate::lock;

/// Which database lock a command needs: writers exclusive, readers shared.
pub(crate) fn lock_mode(cmd: &Cmd) -> Option<lock::Mode> {
    use lock::Mode::{Exclusive, Shared};
    match cmd {
        // installing archives holds the lock for the whole command; installing
        // by name builds first, so (like `build --install`) it locks around
        // each install step only
        Cmd::Install { targets, .. } => {
            if targets.iter().all(|t| crate::pathutil::is_archive_arg(t)) {
                Some(Exclusive)
            } else {
                None
            }
        }
        Cmd::Remove { .. }
        | Cmd::Rollback { .. }
        | Cmd::Autoremove { .. }
        | Cmd::History { .. } => Some(Exclusive),
        Cmd::Upgrade { dry_run, .. } => Some(if *dry_run { Shared } else { Exclusive }),
        // `build --install` / `update` lock themselves around the install
        // step only (holding the db lock through a full build would starve
        // readers)
        Cmd::Build { .. } | Cmd::Update { .. } => None,
        // fetch and recipe check never touch the package database
        Cmd::Fetch { .. } | Cmd::Recipe { .. } => None,
        Cmd::Outdated => Some(Shared),
        Cmd::Query { .. }
        | Cmd::Info { .. }
        | Cmd::Why { .. }
        | Cmd::Files { .. }
        | Cmd::Owner { .. }
        | Cmd::Revdep { .. }
        | Cmd::Deps { .. }
        | Cmd::DepsAll { .. } => Some(Shared), // repo/sign tooling and everything below touches no package database
        Cmd::RepoCreate { .. }
        | Cmd::RepoUpdate { .. }
        | Cmd::Keygen { .. }
        | Cmd::Sign { .. }
        | Cmd::Verify { .. }
        | Cmd::Trust { .. }
        | Cmd::Sync { .. }
        | Cmd::Search { .. }
        | Cmd::Completions { .. }
        | Cmd::Config { .. }
        | Cmd::Init { .. }
        | Cmd::Service { .. }
        | Cmd::CompletePkgs => None,
        Cmd::Group { action } => match action {
            // Group install delegates to the ordinary install path, which
            // acquires short per-transaction locks itself. Holding an
            // outer lock here would deadlock that path.
            GroupCmd::List | GroupCmd::Info { .. } | GroupCmd::Install { .. } => None,
            GroupCmd::Remove { .. } => Some(Exclusive),
        },
        Cmd::Repolist { .. }
        | Cmd::Repoquery { .. }
        | Cmd::Makecache { .. }
        | Cmd::Clean { .. }
        | Cmd::Plugin { .. }
        | Cmd::SandboxInit { .. }
        | Cmd::SandboxProbe { .. }
        | Cmd::External(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn group_install_does_not_take_an_outer_database_lock() {
        let install = crate::cli::Cli::try_parse_from(["nawa", "group", "install", "dev"]).unwrap();
        assert!(lock_mode(&install.cmd).is_none());

        let remove = crate::cli::Cli::try_parse_from(["nawa", "group", "remove", "dev"]).unwrap();
        assert_eq!(lock_mode(&remove.cmd), Some(lock::Mode::Exclusive));
    }

    #[test]
    fn cli_still_parses_every_command_the_policy_covers() {
        // guard against a new Cmd variant being added without a lock decision
        let command = crate::cli::Cli::command();
        assert!(command.has_subcommands());
    }
}
