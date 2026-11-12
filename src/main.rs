//! nawa — source-based package manager
//! A source-based package manager for a from-scratch Linux distribution.

mod archive;
mod artifact;
mod assets;
mod atomic;
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
mod pathutil;
mod plan;
mod plugins;
mod profile;
mod recipe;
mod repo;
mod repositories;
mod revdep;
mod sandbox;
mod service;
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

use anyhow::Result;
use clap::Parser;

use color::{Palette, Stream};
use config::Overrides;

// Items moved out of this file: command-line definition, command handlers and
// formatters. Each module imports what it uses explicitly.
use cli::*;
use commands::*;
use ctx::{resolve_mode, resolve_repo, Ctx};

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
            print_error(e);
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
    if let Err(e) = result {
        // Ctrl+C: the current step was rolled back or finished, never left
        // half-done. Not an error to shout about; exit like a shell would.
        if e.downcast_ref::<interrupt::Interrupted>().is_some() {
            crate::nawa_note_stderr!("interrupted - system state is consistent");
            std::process::exit(130);
        }
        print_error(e);
        std::process::exit(1);
    }
}

/// Whether this command can change the installed package set. Post-transaction
/// triggers are dispatched after these commands, including partial failures.
fn should_run_post_transaction_hooks(cmd: &Cmd) -> bool {
    match cmd {
        Cmd::Build { install, .. } => *install,
        Cmd::Install { .. }
        | Cmd::Update { .. }
        | Cmd::Remove { .. }
        | Cmd::Rollback { .. } => true,
        Cmd::Autoremove { dry_run, .. } => !*dry_run,
        Cmd::Upgrade { dry_run, .. } => !*dry_run,
        Cmd::Group { action } => match action {
            GroupCmd::Install { flags, .. } => !flags.dry_run,
            GroupCmd::Remove { .. } => true,
            GroupCmd::List | GroupCmd::Info { .. } => false,
        },
        Cmd::History { action, .. } => {
            matches!(action, Some(HistoryCmd::Undo { .. }))
        }
        _ => false,
    }
}

/// Compare package payload identity, excluding ownership-only metadata such as
/// the explicit/dependency/group install reason. Shared cache triggers need to
/// run only when package payloads were installed, replaced, or removed.
fn installed_package_payload_changed(before: &[db::Entry], after: &[db::Entry]) -> bool {
    if before.len() != after.len() {
        return true;
    }
    before.iter().zip(after).any(|(old, new)| {
        old.name != new.name
            || old.version != new.version
            || old.rel != new.rel
            || old.arch != new.arch
            || old.install_date != new.install_date
            || old.size != new.size
            || old.artifact_sha256 != new.artifact_sha256
    })
}

/// `nawa: error: ...` plus indented causes and an optional hint; colors only
/// on a real terminal (see `diag::render_error`).
fn print_error(e: anyhow::Error) {
    let p = Palette::auto(Stream::Stderr);
    let rendered = diag::render_error(&e, &p, &diag::Env::current());
    output::write_stderr(&rendered);
}

fn run(ctx: &Ctx, cmd: Cmd, track_package_changes: bool) -> (Result<()>, bool) {
    let snapshot_needs_shared_lock = lock_mode(&cmd).is_none();
    // held until a whole-command transaction completes
    let _db_lock = match lock_mode(&cmd) {
        Some(mode) => match lock::acquire(&ctx.root, mode) {
            Ok(guard) => guard,
            Err(error) => return (Err(error), false),
        },
        None => None,
    };
    // any command that writes under the root first repairs what a killed
    // nawa left behind; holding the exclusive lock guarantees no live
    // transaction is mistaken for a dead one
    if matches!(lock_mode(&cmd), Some(lock::Mode::Exclusive)) {
        install::recover_and_report(&ctx.root);
        // one-shot, idempotent migration of legacy nanosecond install dates
        // (roadmap §4.3): only clearly-nanosecond values are rewritten, each
        // with a .bak copy of the old desc
        let notes = match db::migrate_install_dates(&ctx.root) {
            Ok(notes) => notes,
            Err(error) => return (Err(error), false),
        };
        for note in notes {
            crate::nawa_info_stderr!("migrated: {note}");
        }
    }

    // Commands with whole-operation locks already have a consistent view.
    // Install/update/build commands take short per-step locks, so take a
    // shared lock only while reading each snapshot (never across a full build).
    let package_state_before = if track_package_changes {
        let snapshot = if snapshot_needs_shared_lock {
            match lock::acquire(&ctx.root, lock::Mode::Shared) {
                Ok(_snapshot_lock) => db::Db::new(&ctx.root).installed(),
                Err(error) => Err(error),
            }
        } else {
            db::Db::new(&ctx.root).installed()
        };
        match snapshot {
            Ok(state) => Some(state),
            Err(error) => {
                return (
                    Err(error.context("read installed package state before command")),
                    false,
                );
            }
        }
    } else {
        None
    };

    let command_result = run_command(ctx, cmd);

    let package_state_changed = match package_state_before {
        Some(before) => {
            let snapshot = if snapshot_needs_shared_lock {
                match lock::acquire(&ctx.root, lock::Mode::Shared) {
                    Ok(_snapshot_lock) => db::Db::new(&ctx.root).installed(),
                    Err(error) => Err(error),
                }
            } else {
                db::Db::new(&ctx.root).installed()
            };
            match snapshot {
                Ok(after) => installed_package_payload_changed(&before, &after),
                Err(error) => {
                    crate::nawa_warn!(
                        "could not compare installed packages after command ({error:#}); running transaction triggers conservatively"
                    );
                    true
                }
            }
        }
        None => false,
    };

    (command_result, package_state_changed)
}

fn run_command(ctx: &Ctx, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Build {
            name,
            check,
            check_nonfatal,
            isolation,
            rebuild_check,
            allow_network,
            allow_degraded,
            timeout,
            use_cache,
            offline,
            install,
            force,
        } => {
            let mode = if check_nonfatal {
                build::CheckMode::Warn
            } else if check {
                build::CheckMode::Fatal
            } else {
                build::CheckMode::Off
            };
            cmd_build(
                ctx,
                &name,
                mode,
                isolation.as_deref(),
                rebuild_check,
                allow_network,
                allow_degraded,
                timeout,
                use_cache,
                offline,
                install,
                force,
            )
        }
        Cmd::Install { targets, flags, no_deps, key, build, from_repo, from, repo } => {
            let mode = resolve_mode(build, from_repo, from.as_deref())?;
            cmd_install(ctx, &targets, &flags, no_deps, key, mode, repo)
        }
        Cmd::Outdated => cmd_outdated(ctx),
        Cmd::Update { names, flags, build, from_repo, from, repo } => {
            let mode = resolve_mode(build, from_repo, from.as_deref())?;
            cmd_update(ctx, &names, &flags, mode, repo)
        },
        Cmd::Autoremove { dry_run, yes, build_deps } => {
            cmd_autoremove(ctx, dry_run, yes, build_deps)
        }
        Cmd::Search { term, glob } => cmd_search(ctx, &term, glob),
        Cmd::Info { name } => cmd_info(ctx, &name),
        Cmd::Why { name, tree } => cmd_why(ctx, &name, tree),
        Cmd::Deps { name } => cmd_deps(ctx, &name),
        Cmd::DepsAll { include_installed } => cmd_deps_all(ctx, include_installed),
        Cmd::Remove { name, force, yes } => cmd_remove(ctx, &name, force, yes),
        Cmd::Query { pattern, explicit, deps } => {
            cmd_query(ctx, pattern.as_deref(), explicit, deps)
        }
        Cmd::Files { name } => cmd_files(ctx, &name),
        Cmd::Owner { file } => cmd_owner(ctx, &file),
        Cmd::Revdep { names, lib_dirs, ignore, no_symlinks, unowned, scan_dirs, json, no_suggest } => {
            cmd_revdep(ctx, &names, &lib_dirs, &ignore, no_symlinks, unowned, &scan_dirs, json, no_suggest)
        }
        Cmd::RepoCreate { dir, out, sign, key, index_version, locations, recipes, recipes_rev, groups } => {
            cmd_repo_create(&dir, out, sign, key.as_deref(), index_version, locations.as_deref(), recipes.as_deref(), recipes_rev.as_deref(), groups.as_deref())
        }
        Cmd::RepoUpdate { dir, base, trust_key, out, add_entries, remove, drop, locations, recipes, recipes_rev, key } => {
            cmd_repo_update(&dir, &base, &trust_key, out, add_entries.as_deref(), remove, drop, locations.as_deref(), recipes.as_deref(), recipes_rev.as_deref(), key.as_deref())
        }
        Cmd::Keygen { out } => cmd_keygen(out),
        Cmd::Sign { file, key } => cmd_sign(&file, key.as_deref()),
        Cmd::Verify { file, key } => {
            sign::verify_standalone(&file, &ctx.root, key.as_deref())?;
            Ok(())
        }
        Cmd::Trust { keyfile } => cmd_trust(ctx, &keyfile),
        Cmd::Upgrade { repo, dry_run, force, allow_unsigned, yes } => {
            let repo_ref = resolve_repo(ctx, repo)?;
            upgrade::upgrade(
                &ctx.root,
                &repo_ref,
                &ctx.recipes,
                &upgrade::UpgradeOpts { dry_run, force, allow_unsigned, yes },
            )
        }
        Cmd::Sync { repo, force, allow_unsigned } => {
            let repo_ref = resolve_repo(ctx, repo)?;
            sync::sync(&ctx.root, &repo_ref, &ctx.recipes, &sync::SyncOpts { force, allow_unsigned })
        }
        Cmd::Repolist { all } => repositories::cmd_repolist(&ctx.cfg, all),
        Cmd::Makecache { allow_unsigned, force } => {
            repositories::cmd_makecache(&ctx.cfg, &ctx.root, allow_unsigned, force)
        }
        Cmd::Repoquery { name, repo_id, info } => {
            repositories::cmd_repoquery(&ctx.cfg, &ctx.root, &name, repo_id.as_deref(), info)
        }
        Cmd::Fetch { names, all, isolation, sources_only, offline, allow_degraded } => {
            cmd_fetch_checked(ctx, &names, all, isolation.as_deref(), sources_only, offline, allow_degraded)
        }
        Cmd::Recipe { action } => cmd_recipe(ctx, action),
        Cmd::Clean { category, dry_run } => repositories::cmd_clean(ctx, &category, dry_run),
        Cmd::Group { action } => match action {
            GroupCmd::List => cmd_group_list(ctx),
            GroupCmd::Info { id } => cmd_group_info(ctx, &id),
            GroupCmd::Install { id, with_optional, flags } => cmd_group_install(ctx, &id, &flags, with_optional),
            GroupCmd::Remove { id, yes } => cmd_group_remove(ctx, &id, yes),
        },
        Cmd::Plugin { action } => match action {
            PluginCmd::List => cmd_plugin_list(ctx),
            PluginCmd::Info { id } => cmd_plugin_info(ctx, &id),
            PluginCmd::Enable { id } => cmd_plugin_enable(ctx, &id, true),
            PluginCmd::Disable { id } => cmd_plugin_enable(ctx, &id, false),
            PluginCmd::Grant { id, capability } => cmd_plugin_grant(ctx, &id, &capability, true),
            PluginCmd::Revoke { id, capability } => cmd_plugin_grant(ctx, &id, &capability, false),
            PluginCmd::Run { id, command, args } => cmd_plugin_run(ctx, &id, &command, &args),
        },
        Cmd::External(values) => cmd_plugin_external(ctx, values),
        Cmd::Rollback { name } => install::rollback(&name, &ctx.root),
        Cmd::History { action, limit } => cmd_history(ctx, action, limit),
        Cmd::Service { action } => cmd_service(ctx, action),
        Cmd::Config { action } => match action {
            ConfigCmd::Show => cmd_config_show(ctx),
        },
        Cmd::Init { force, system } => cmd_init(force, system),
        Cmd::Completions { shell } => {
            print!("{}", completions_script(shell));
            Ok(())
        }
        Cmd::CompletePkgs => cmd_complete_pkgs(ctx),
        Cmd::SandboxInit { spec } => {
            let code = sandbox::init(&spec)?;
            std::process::exit(code);
        }
        Cmd::SandboxProbe { op } => cmd_sandbox_probe(op),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(name: &str) -> db::Entry {
        db::Entry {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            rel: 1,
            arch: "x86_64".to_string(),
            reason: "explicit".to_string(),
            run: vec!["glibc".to_string()],
            install_date: 1_700_000_000,
            size: 4096,
            artifact_sha256: "a".repeat(64),
        }
    }

    #[test]
    fn transaction_triggers_only_follow_installed_payload_changes() {
        let before = vec![sample_entry("desktop-file-utils")];

        let unchanged = vec![sample_entry("desktop-file-utils")];
        assert!(!installed_package_payload_changed(&before, &unchanged));

        let mut reason_only = sample_entry("desktop-file-utils");
        reason_only.reason = "group".to_string();
        assert!(!installed_package_payload_changed(&before, &[reason_only]));

        assert!(installed_package_payload_changed(&[], &before));
        assert!(installed_package_payload_changed(&before, &[]));

        let mut upgraded = sample_entry("desktop-file-utils");
        upgraded.rel = 2;
        upgraded.install_date += 1;
        upgraded.artifact_sha256 = "b".repeat(64);
        assert!(installed_package_payload_changed(&before, &[upgraded]));
    }
}
