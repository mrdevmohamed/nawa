//! nawa — source-based package manager
//! A source-based package manager for a from-scratch Linux distribution.

mod archive;
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
mod fetch;
mod humanize;
mod install;
mod lint;
mod lock;
mod plan;
mod profile;
mod recipe;
mod repo;
mod revdep;
mod sandbox;
mod service;
mod sign;
mod slim;
mod suggest;
mod sync;
mod sys;
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
    if let Err(e) = run(&ctx, cmd) {
        print_error(e);
        std::process::exit(1);
    }
}

/// `nawa: error: ...` — red "error:" only on a real terminal.
fn print_error(e: anyhow::Error) {
    let p = Palette::auto(Stream::Stderr);
    // "error:" carries its own colon; the palette adds color, not punctuation
    eprintln!("nawa: {} {e:#}", p.err("error:"));
}

fn run(ctx: &Ctx, cmd: Cmd) -> Result<()> {
    // held until `run` returns
    let _db_lock = match lock_mode(&cmd) {
        Some(mode) => lock::acquire(&ctx.root, mode)?,
        None => None,
    };
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
        Cmd::Update { names, flags } => cmd_update(ctx, &names, &flags),
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
        Cmd::Revdep { names, lib_dirs, ignore, no_symlinks, unowned, scan_dirs } => {
            cmd_revdep(ctx, &names, &lib_dirs, &ignore, no_symlinks, unowned, &scan_dirs)
        }
        Cmd::RepoCreate { dir, out, sign, key, recipes, recipes_rev } => {
            // a relative --out lands inside the repo dir, not in the CWD
            let out = if out.is_absolute() { out } else { dir.join(out) };
            let tree = match (&recipes, &recipes_rev) {
                (Some(t), Some(r)) => Some((t.as_path(), r.as_str())),
                _ => None,
            };
            repo::create_index_with(&dir, &out, tree)?;
            if sign {
                let kp = sign::resolve_signing_key(key.as_deref())?;
                let sp = sign::sign(&out, &kp)?;
                println!("nawa: signed index -> {}", sp.display());
            }
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
        Cmd::Rollback { name } => install::rollback(&name, &ctx.root),
        Cmd::Service { action } => match action {
            ServiceCmd::List => service::print_list(&ctx.root),
            ServiceCmd::Enable { name } => {
                service::enable(&ctx.root, &name)?;
                println!(
                    "nawa: enabled {name} — runsvdir picks it up within seconds \
                     (use `sv up {name}` to start it right now)"
                );
                Ok(())
            }
            ServiceCmd::Disable { name } => {
                service::disable(&ctx.root, &name)?;
                println!(
                    "nawa: disabled {name} — runsvdir stops it within seconds \
                     (use `sv down {name}` to stop it right now)"
                );
                Ok(())
            }
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
