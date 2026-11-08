//! nawa — source-based package manager
//! A source-based package manager for a from-scratch Linux distribution.

mod archive;
mod artifact;
mod assets;
mod build;
mod buildenv;
mod cli;
mod closure;
mod color;
mod commands;
mod config;
mod confirm;
mod db;
mod deps;
mod diag;
mod fetch;
mod fetchrun;
mod groups;
mod humanize;
mod install;
mod interrupt;
mod lint;
mod lock;
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
mod transaction;
mod upgrade;
mod version;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use color::{Palette, Stream};
use config::{Config, InstallMode, Overrides};

// Items moved out of this file: command-line definition, command handlers and
// formatters. Re-imported here (and, through `use crate::*`, in those modules).
use cli::*;
use commands::*;
use humanize::*;

struct Ctx {
    root: PathBuf,
    recipes: PathBuf,
    builddir: PathBuf,
    profiles: PathBuf,
    jobs: Option<u32>,
    repo: Option<String>,
    install_mode: InstallMode,
    /// the resolved configuration, kept for `config show` (sources + file)
    cfg: Config,
}

impl Ctx {
    fn from_config(cfg: &Config) -> Self {
        Ctx {
            root: cfg.root.clone(),
            recipes: cfg.recipes.clone(),
            builddir: cfg.builddir.clone(),
            profiles: cfg.profiles.clone(),
            jobs: cfg.jobs,
            repo: cfg.repo.clone(),
            install_mode: cfg.install_mode,
            cfg: cfg.clone(),
        }
    }
}

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
    let result = run(&ctx, cmd);
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
            eprintln!("nawa: interrupted - the system was left consistent");
            std::process::exit(130);
        }
        print_error(e);
        std::process::exit(1);
    }
}

/// `nawa: error: ...` plus indented causes and an optional hint; colors only
/// on a real terminal (see `diag::render_error`).
fn print_error(e: anyhow::Error) {
    let p = Palette::auto(Stream::Stderr);
    eprint!("{}", diag::render_error(&e, &p, &diag::Env::current()));
}

fn run(ctx: &Ctx, cmd: Cmd) -> Result<()> {
    // held until `run` returns
    let _db_lock = match lock_mode(&cmd) {
        Some(mode) => lock::acquire(&ctx.root, mode)?,
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
        for note in db::migrate_install_dates(&ctx.root)? {
            eprintln!("nawa: migrated: {note}");
        }
    }
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
            let mode = if build {
                Some(InstallMode::Build)
            } else if from_repo {
                Some(InstallMode::Repo)
            } else if let Some(m) = from {
                Some(InstallMode::parse(&m)?)
            } else {
                None
            };
            cmd_install(ctx, &targets, &flags, no_deps, key, mode, repo)
        }
        Cmd::Outdated => cmd_outdated(ctx),
        Cmd::Update { names, flags, build, from_repo, from, repo } => {
            let mode = if build {
                Some(InstallMode::Build)
            } else if from_repo {
                Some(InstallMode::Repo)
            } else if let Some(m) = from {
                Some(InstallMode::parse(&m)?)
            } else {
                None
            };
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
        Cmd::RepoCreate { dir, out, sign, key, index_version, locations, recipes, recipes_rev } => {
            if index_version >= repo::INDEX_SCHEMA_V2 && !sign {
                bail!("repo-create --index-version {index_version} requires --sign; repository indexes v2 and newer must be authenticated");
            }
            if index_version == repo::INDEX_SCHEMA_V3 && locations.is_none() {
                bail!("repo-create --index-version 3 requires --locations");
            }
            if index_version != repo::INDEX_SCHEMA_V3 && locations.is_some() {
                bail!("repo-create --locations is only valid with --index-version 3");
            }
            // Resolve the key before writing anything, so a missing signing
            // key cannot leave an unsigned v2 index in the repository.
            let signing_key = if sign {
                let path = sign::resolve_signing_key(key.as_deref())?;
                sign::load_signing_key(&path)
                    .with_context(|| format!("invalid repository signing key {}", path.display()))?;
                Some(path)
            } else {
                None
            };
            // A relative --out lands inside the repo dir, not in the CWD.
            let out = if out.is_absolute() { out } else { dir.join(out) };
            let tree = match (&recipes, &recipes_rev) {
                (Some(t), Some(r)) => Some((t.as_path(), r.as_str())),
                _ => None,
            };
            match locations.as_deref() {
                Some(path) => repo::create_index_with_locations(&dir, &out, tree, index_version, Some(path))?,
                None => repo::create_index_with_version(&dir, &out, tree, index_version)?,
            };
            if let Some(kp) = signing_key {
                let sp = sign::sign(&out, &kp)?;
                println!("nawa: signed index -> {}", sp.display());
            }
            Ok(())
        }
        Cmd::RepoUpdate { dir, base, trust_key, out, add_entries, remove, drop, locations, recipes, recipes_rev, key } => {
            // Resolve the signing key first: an update is always signed, and a
            // missing key must not leave an unsigned index behind.
            let signing_key = sign::resolve_signing_key(key.as_deref())?;
            sign::load_signing_key(&signing_key)
                .with_context(|| format!("invalid repository signing key {}", signing_key.display()))?;
            // Verify the exact bytes that are then parsed: an unverified base
            // must never flow into a signed index.
            let vk = sign::load_verify_key(&trust_key)?;
            let base_bytes = std::fs::read(&base)
                .with_context(|| format!("read base index {}", base.display()))?;
            let sf = sign::read_sig_file(&sign::sig_path_for(&base))?;
            sign::verify_bytes(&base_bytes, &vk, &sf.sig).with_context(|| {
                format!("base index {} is not signed by {}; refusing to update an unverified index", base.display(), trust_key.display())
            })?;
            let base_index = repo::load_index_bytes(&base_bytes)
                .with_context(|| format!("parse base index {}", base.display()))?;
            let out = if out.is_absolute() { out } else { dir.join(out) };
            let tree = match (&recipes, &recipes_rev) {
                (Some(t), Some(r)) => Some((t.as_path(), r.as_str())),
                _ => None,
            };
            repo::update_index_files(
                &base_index,
                &dir,
                &out,
                repo::UpdateIndexFilesInputs {
                    entries_path: add_entries.as_deref(),
                    remove_files: remove,
                    drop_packages: drop,
                    locations_path: locations.as_deref(),
                    recipes: tree,
                },
            )?;
            let sp = sign::sign(&out, &signing_key)?;
            println!("nawa: signed index -> {}", sp.display());
            Ok(())
        }
        Cmd::Keygen { out } => {
            let dir = out.unwrap_or_else(sign::default_key_dir);
            let (sec, pub_) = sign::keygen(&dir)?;
            println!("nawa: secret key  {}  (keep safe, mode 0600)", sec.display());
            println!("nawa: public key  {}  (share / `nawa trust` on targets)", pub_.display());
            Ok(())
        }
        Cmd::Sign { file, key } => {
            let kp = sign::resolve_signing_key(key.as_deref())?;
            let sp = sign::sign(&file, &kp)?;
            println!("nawa: signed {} -> {}", file.display(), sp.display());
            Ok(())
        }
        Cmd::Verify { file, key } => {
            sign::verify_standalone(&file, &ctx.root, key.as_deref())?;
            Ok(())
        }
        Cmd::Trust { keyfile } => {
            let dest = sign::trust(&keyfile, &ctx.root)?;
            println!("nawa: trusted key installed at {}", dest.display());
            Ok(())
        }
        Cmd::Upgrade { repo, dry_run, force, allow_unsigned, yes } => {
            let repo = match repo.or_else(|| ctx.repo.clone()) {
                Some(r) => r,
                None => bail!(
                    "no repository configured (it is switched off with `repo = \"\"`): \
                     pass --repo <dir-or-url>, set `repo` in nawa.toml or \
                     /etc/nawa/nawa.toml, or export NAWA_REPO"
                ),
            };
            let repo_ref = fetch::RepoRef::parse(&repo)?;
            upgrade::upgrade(
                &ctx.root,
                &repo_ref,
                &ctx.recipes,
                &upgrade::UpgradeOpts { dry_run, force, allow_unsigned, yes },
            )
        }
        Cmd::Sync { repo, force, allow_unsigned } => {
            let repo = match repo.or_else(|| ctx.repo.clone()) {
                Some(r) => r,
                None => bail!(
                    "no repository configured (it is switched off with `repo = \"\"`): \
                     pass --repo <dir-or-url>, set `repo` in nawa.toml or \
                     /etc/nawa/nawa.toml, or export NAWA_REPO"
                ),
            };
            let repo_ref = fetch::RepoRef::parse(&repo)?;
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
            if !all && names.is_empty() {
                bail!("`nawa fetch` needs recipe names or --all");
            }
            cmd_fetch(
                ctx,
                &names,
                all,
                isolation.as_deref(),
                sources_only,
                offline,
                allow_degraded,
            )
        }
        Cmd::Recipe { action } => match action {
            RecipeCmd::Check { name, all } => {
                if name.is_none() && !all {
                    bail!("`nawa recipe check` needs a recipe name or --all");
                }
                if name.is_some() && all {
                    bail!("a recipe name and --all are exclusive");
                }
                cmd_recipe_check(ctx, name.as_deref(), all)
            }
        },
        Cmd::Clean { category, dry_run } => repositories::cmd_clean(ctx, &category, dry_run),
        Cmd::Group { action } => match action {
            GroupCmd::List => cmd_group_list(ctx),
            GroupCmd::Info { id } => cmd_group_info(ctx, &id),
            GroupCmd::Install { id } => cmd_group_install(ctx, &id),
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
        Cmd::External(values) => {
            let mut values = values.into_iter();
            let command = values.next()
                .ok_or_else(|| anyhow::anyhow!("missing plugin command name"))?
                .into_string()
                .map_err(|_| anyhow::anyhow!("plugin command name is not valid UTF-8"))?;
            let args: Result<Vec<String>> = values
                .map(|value| value.into_string().map_err(|_| anyhow::anyhow!("plugin arguments must be valid UTF-8 for JSONL")))
                .collect();
            cmd_plugin_dispatch(ctx, &command, &args?)
        },
        Cmd::Rollback { name } => install::rollback(&name, &ctx.root),
        Cmd::History { action, limit } => {
            // the lock is already held (see lock_mode); reconcile what a
            // killed nawa left behind before showing or undoing anything
            for note in transaction::recovery::reconcile_incomplete(&ctx.root)? {
                eprintln!("nawa: recovered: {note}");
            }
            match action {
                None => transaction::undo::print_history(&ctx.root, limit.unwrap_or(20)),
                Some(HistoryCmd::List) => transaction::undo::print_history(&ctx.root, limit.unwrap_or(20)),
                Some(HistoryCmd::Info { id }) => transaction::undo::print_info(&ctx.root, id),
                Some(HistoryCmd::Undo { id, yes }) => transaction::undo::history_undo(&ctx.root, id, yes),
            }
        }
        Cmd::Service { action } => match action {
            ServiceCmd::List => service::print_list(&ctx.root),
            ServiceCmd::Enable { name, now } => {
                if now {
                    service::enable_now(&ctx.root, &name)?;
                } else {
                    service::enable(&ctx.root, &name)?;
                    println!(
                        "nawa: enabled {name} — runsvdir picks it up within ~5 s \
                         (or: `nawa service start {name}`)"
                    );
                }
                Ok(())
            }
            ServiceCmd::Disable { name, now } => {
                if now {
                    service::disable_now(&ctx.root, &name)?;
                    println!("nawa: stopped and disabled {name}");
                } else {
                    service::disable(&ctx.root, &name)?;
                    println!(
                        "nawa: disabled {name} — runsvdir stops it within ~5 s \
                         (or use `nawa service disable --now {name}`)"
                    );
                }
                Ok(())
            }
            ServiceCmd::Start { name } => service::start(&ctx.root, &name),
            ServiceCmd::Stop { name } => service::stop(&ctx.root, &name),
            ServiceCmd::Restart { name } => service::restart(&ctx.root, &name),
            ServiceCmd::Status { name } => service::print_status(&ctx.root, name.as_deref()),
        },
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
        Cmd::SandboxProbe { op } => {
            // a single probe runs in this process; the full mode spawns one
            // process per op, because each probe creates namespaces and
            // nesting them in one process corrupts the results
            match op {
                Some(o) => {
                    let ok = match sandbox::probe(&o) {
                        Ok(true) => {
                            println!("PASS  {o}");
                            true
                        }
                        Ok(false) => {
                            println!("FAIL  {o}");
                            false
                        }
                        Err(e) => {
                            println!("ERROR {o}: {e:#}");
                            false
                        }
                    };
                    std::process::exit(if ok { 0 } else { 1 });
                }
                None => {
                    let exe = std::env::current_exe().context("locate the nawa binary")?;
                    let mut all_ok = true;
                    for o in sandbox::PROBES {
                        let out = std::process::Command::new(&exe)
                            .arg("sandbox-probe")
                            .arg(o)
                            .output()
                            .with_context(|| format!("run probe {o}"))?;
                        let text = String::from_utf8_lossy(&out.stdout);
                        let one_line = text
                            .lines()
                            .find(|l| l.starts_with("PASS") || l.starts_with("FAIL"))
                            .unwrap_or_default();
                        println!("{one_line}");
                        if !out.status.success() {
                            all_ok = false;
                        }
                    }
                    if all_ok {
                        println!("sandbox probe: OK — all features available");
                    } else {
                        println!("sandbox probe: some features are missing (restricted kernel/container?)");
                        println!(
                            "nawa build refuses to weaken isolation silently; pass \
                             --allow-degraded to continue"
                        );
                    }
                    std::process::exit(if all_ok { 0 } else { 1 });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matching() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("gz*", "gzip"));
        assert!(!glob_match("gz*", "zstd"));
        assert!(!glob_match("z?d", "zstd")); // 4 chars vs 3
        assert!(glob_match("z?d", "zad"));
        assert!(glob_match("*std*", "zstd"));
        assert!(glob_match("GZ*", "gzip"), "case-insensitive");
        assert!(glob_match("zstd", "zstd"));
        assert!(!glob_match("", "zstd"));
        assert!(glob_match("", ""));
    }

    #[test]
    fn date_formatting() {
        assert_eq!(format_date(0), "-");
        assert_eq!(format_date(86_400), "1970-01-02");
        assert_eq!(format_date(1_700_000_000), "2023-11-14");
        assert_eq!(format_date(1_704_067_200), "2024-01-01");
        assert_eq!(format_date(951_782_400), "2000-02-29"); // leap day
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(185), "3m05s");
        assert_eq!(format_duration(3720), "1h02m");
    }

    #[test]
    fn archive_vs_name_arguments() {
        assert!(is_archive_arg("build/out/a-1-1.nawa.tar.zst"));
        assert!(is_archive_arg("./a"));
        assert!(is_archive_arg("a-1-1.nawa.tar.zst"));
        assert!(!is_archive_arg("gzip"));
        assert!(!is_archive_arg("libstdc++"));
    }

    #[test]
    fn size_formatting() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KiB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
