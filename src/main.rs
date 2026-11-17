//! nawa — source-based package manager
//! A source-based package manager for a from-scratch Linux distribution.

mod application;
mod archive;
mod artifact;
mod assets;
mod atomic;
mod bootstrap;
mod build;
mod buildenv;
mod cli;
mod checksum;
mod closure;
mod color;
mod output;
mod commands;
mod config;
mod ctx;
mod confirm;
mod db;
mod deps;
mod diag;
mod domain;
mod error;
mod fetch;
mod fetchrun;
mod fswalk;
mod glob;
mod groups;
mod humanize;
mod install;
mod interrupt;
mod lint;
mod lock;
mod migration_record;
mod pathutil;
mod plan;
mod plugins;
mod profile;
mod recipe;
mod recovery;
mod repo;
mod repositories;
mod revdep;
mod sandbox;
mod sign;
mod slim;
mod sources;
mod suggest;
mod sync;
mod sys;
#[cfg(test)]
mod testutil;
mod transaction;
mod undo;
mod unique;
mod upgrade;
mod version;

// Items moved out of this file: the command-line definition (`cli`), the
// top-level command orchestration and dispatch (`commands::dispatch`) and
// the error/exit path (`bootstrap`). Each module imports what it uses
// explicitly.
use clap::Parser;
use commands::{run, should_run_post_transaction_hooks};
use config::Overrides;
use ctx::Ctx;

// The cli items stay bound at the crate root (as before the cli module
// split): `commands/fetching.rs` addresses `crate::default_jobs`, and the
// blanket binding preserves that without re-exports.
use cli::*;

fn main() {
    let Cli { root, recipes, builddir, profiles, jobs, cmd } = Cli::parse();
    let overrides = Overrides {
        root,
        recipes,
        builddir,
        profiles,
        jobs,
        repo: None,
        install_mode: None,
        downloads: None,
    };
    // config errors happen before anything else; report them in style
    let cfg = match config::load(&overrides) {
        Ok(c) => c,
        Err(e) => {
            bootstrap::print_error(e);
            std::process::exit(1);
        }
    };
    let ctx = Ctx::from_config(&cfg);
    // Package lifecycle hooks already run under their parent's package
    // database lock. A nested Nawa invocation from one of those hooks must
    // not dispatch system triggers before the outer package transaction ends.
    let hooks_suppressed = std::env::var_os(lock::HELD_ENV).is_some()
        || std::env::var_os("NAWA_POST_TRANSACTION_HOOKS_RUNNING").is_some();
    let run_transaction_hooks = should_run_post_transaction_hooks(&cmd) && !hooks_suppressed;
    let (result, package_state_changed) = run(&ctx, cmd, run_transaction_hooks);
    // Distribution-provided post-transaction triggers (for example, MIME and
    // desktop application cache rebuilds) run once after the package command
    // releases its lock. Run them even after a partial transaction failure:
    // packages committed before the failure still changed the live system.
    if run_transaction_hooks && package_state_changed {
        if let Err(error) = install::hooks::run_post_transaction_hooks(&ctx.root) {
            crate::nawa_warn!(
                "post-transaction maintenance failed ({error:#}); it will be retried after the next package transaction"
            );
        }
    }
    // Observer plugins execute only after run() has dropped the package DB
    // lock; a slow notification must never block a package commit.
    plugins::events::dispatch_pending(
        &ctx.root,
        ctx.cfg.file.as_deref(),
        ctx.cfg.system_file.as_deref(),
    );
    bootstrap::exit_on_error(result);
}
