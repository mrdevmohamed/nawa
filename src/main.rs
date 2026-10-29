//! nawa — Distro Package Manager
//! A source-based package manager for a from-scratch Linux distribution.

mod archive;
mod build;
mod buildenv;
mod closure;
mod color;
mod config;
mod confirm;
mod db;
mod deps;
mod fetch;
mod install;
mod lint;
mod lock;
mod plan;
mod profile;
mod recipe;
mod repo;
mod sandbox;
mod service;
mod sign;
mod suggest;
mod upgrade;
mod version;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use color::{Palette, Stream};
use config::{Config, Overrides};

#[derive(Parser)]
#[command(
    name = "nawa",
    version,
    about = "nawa — source-based package manager for a from-scratch Linux distribution",
    after_help = "Global flags (--root --recipes --builddir --profiles --jobs) work \
                  anywhere on the line and may come from $NAWA_* variables or a nawa.toml \
                  config file (see `nawa init`, inspect with `nawa config show`).

Examples:
  nawa search zip                        find recipes matching \"zip\"
  nawa info zstd                         full package details
  nawa install curl                      build curl AND its missing dependencies
  nawa install -n curl                   show that plan, change nothing
  nawa outdated / nawa update             see and apply newer recipe versions
  nawa autoremove                        drop dependencies nothing needs
  nawa build --install curl              build and install ONE recipe
  nawa install --root /mnt/root a.nawa.tar.zst
  nawa why --tree curl                   who needs curl (installed + recipe tree)"
)]
struct Cli {
    /// Target root filesystem (use a throwaway dir for testing)
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// Root of the recipes tree
    #[arg(long, global = true)]
    recipes: Option<PathBuf>,

    /// Scratch/build directory
    #[arg(long, global = true)]
    builddir: Option<PathBuf>,

    /// Directory with per-architecture build profiles (`<arch>.toml`)
    #[arg(long, global = true)]
    profiles: Option<PathBuf>,

    /// Parallel build jobs (default: the profile's `jobs`, else the CPU count)
    #[arg(long, global = true)]
    jobs: Option<u32>,

    #[command(subcommand)]
    cmd: Cmd,
}

fn default_jobs() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
}

#[derive(Subcommand)]
enum Cmd {
    /// Build a package from its recipe (by name inside --recipes)
    Build {
        name: String,
        /// Run the recipe's `check` section (test suite) after compiling;
        /// a failing check aborts the build
        #[arg(long)]
        check: bool,
        /// Like --check, but a failing test suite only prints a warning
        #[arg(long)]
        check_nonfatal: bool,
        /// Isolation level for the build: none | env | sandbox | closure
        /// (default: the profile's [build] isolation, else none)
        #[arg(long)]
        isolation: Option<String>,
        /// Build twice in different directories and compare archive hashes
        /// (reproducibility check; implies at least env isolation)
        #[arg(long)]
        rebuild_check: bool,
        /// Permit a recipe's [sandbox] network = true (recorded as impure)
        #[arg(long)]
        allow_network: bool,
        /// Continue when the host kernel refuses an isolation feature
        /// (by default the build stops instead of silently weakening)
        #[arg(long)]
        allow_degraded: bool,
        /// Per-section wall-clock limit in seconds (sandbox levels)
        #[arg(long)]
        timeout: Option<u64>,
        /// Skip the build when an archive for this package already exists
        /// and its recorded build_id matches the current inputs
        /// (recipe bytes, source sha256, closure, profile flags)
        #[arg(long)]
        use_cache: bool,
        /// Install the freshly built archive into --root when the build
        /// succeeds (replaces the manual find-the-archive + install dance)
        #[arg(long)]
        install: bool,
        /// With --install: overwrite files owned by other packages
        #[arg(long, requires = "install")]
        force: bool,
    },

    /// Install packages by name (builds them and every missing dependency
    /// from the recipes, after showing the plan) or install built
    /// .nawa.tar.zst archives
    Install {
        /// package names (`nawa install curl`) or archive paths
        /// (`nawa install build/out/curl-8.9-1.nawa.tar.zst`)
        #[arg(required = true)]
        targets: Vec<String>,
        #[command(flatten)]
        flags: PlanFlags,
        /// By name: build only the named packages, skip their dependencies
        #[arg(long)]
        no_deps: bool,
        /// Archives only: verify the signature against this public key
        /// instead of the trust store
        #[arg(long)]
        key: Option<PathBuf>,
    },

    /// List installed packages whose recipe has a newer version
    Outdated,

    /// Rebuild and reinstall outdated packages from the recipes (all of
    /// them, or just the names given); shows the plan first
    Update {
        /// only update these packages (default: every outdated one)
        names: Vec<String>,
        #[command(flatten)]
        flags: PlanFlags,
    },

    /// Remove dependency packages that nothing needs any more
    Autoremove {
        /// show what would be removed, change nothing
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// do not ask for confirmation (non-interactive runs REQUIRE this)
        #[arg(short = 'y', long)]
        yes: bool,
        /// also remove build-only dependencies (compilers, -dev libraries);
        /// by default those are kept because the next rebuild needs them
        #[arg(long)]
        build_deps: bool,
    },

    /// Search the recipes tree by name or summary (substring, or --glob)
    Search {
        term: String,
        /// Match the name against a glob pattern (`*` and `?`), not a substring
        #[arg(long)]
        glob: bool,
    },

    /// Show everything known about a recipe and its installed state
    Info { name: String },

    /// Show which installed packages (and, with --tree, which recipes)
    /// depend on a package — the reverse of `deps`
    Why {
        name: String,
        /// also scan the whole recipes tree for dependents
        #[arg(long)]
        tree: bool,
    },

    /// Print the dependency resolution order for a package
    Deps { name: String },

    /// Print the global build order for ALL recipes in the tree
    /// (one bare package name per line — script-friendly). Packages already
    /// recorded in the local db are skipped (use --include-installed to list
    /// them too, e.g. for a full world rebuild).
    DepsAll {
        /// also list packages already recorded in the local db
        /// (the default skips them, which is what a resuming stage2 wants;
        /// a stage3 world rebuild needs every recipe)
        #[arg(long)]
        include_installed: bool,
    },

    /// Remove an installed package (asks for confirmation unless --yes)
    Remove {
        name: String,
        /// remove even if other packages depend on it
        #[arg(long)]
        force: bool,
        /// do not ask for confirmation (non-interactive runs REQUIRE this)
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// List installed packages (optionally filtered by name substring)
    #[command(alias = "list")]
    Query {
        /// only show packages whose name contains this (case-insensitive)
        pattern: Option<String>,
        /// only packages you installed yourself (not pulled in as dependencies)
        #[arg(short = 'e', long, conflicts_with = "deps")]
        explicit: bool,
        /// only packages that were installed as dependencies
        #[arg(short = 'd', long)]
        deps: bool,
    },

    /// List files owned by an installed package
    Files { name: String },

    /// Find which installed package owns a file
    Owner { file: String },

    /// Create a repository index from a directory of archives
    #[command(name = "repo-create")]
    RepoCreate {
        dir: PathBuf,
        #[arg(long, default_value = "index.toml")]
        out: PathBuf,
        /// Sign index.toml after writing it
        #[arg(long)]
        sign: bool,
        /// Signing key for --sign
        #[arg(long)]
        key: Option<PathBuf>,
    },

    /// Generate an ed25519 signing keypair (nawa.sec + nawa.pub)
    Keygen {
        /// Directory for the keypair (default: /etc/nawa/keys or ~/.config/nawa/keys)
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Sign a package archive or index.toml (writes <file>.sig)
    Sign {
        file: PathBuf,
        /// Signing key (default: NAWA_SIGN_KEY / /etc/nawa/keys/nawa.sec)
        #[arg(long)]
        key: Option<PathBuf>,
    },

    /// Verify a signature on a package archive or index
    Verify {
        file: PathBuf,
        /// Verify against this public key (default: trust store, then embedded key)
        #[arg(long)]
        key: Option<PathBuf>,
    },

    /// Add a public key to the trust store (<root>/etc/nawa/trusted-keys)
    Trust { keyfile: PathBuf },

    /// Upgrade installed packages from a repository (index.toml); shows the
    /// plan and asks for confirmation unless --yes is passed
    Upgrade {
        /// Repository directory or http(s) URL containing index.toml
        /// (default: the `repo` key in nawa.toml / $NAWA_REPO)
        #[arg(long)]
        repo: Option<String>,
        /// Show the upgrade plan without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Overwrite files owned by other packages
        #[arg(long)]
        force: bool,
        /// Accept unsigned archives when trust keys exist
        #[arg(long)]
        allow_unsigned: bool,
        /// do not ask for confirmation (non-interactive runs REQUIRE this)
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Roll a package back to the version recorded in its undo log
    Rollback { name: String },

    /// Manage runit services (enable/disable/list/status)
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },

    /// Show the effective configuration and where each value comes from
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },

    /// Write a commented nawa.toml template into the current directory
    Init {
        /// overwrite an existing nawa.toml
        #[arg(long)]
        force: bool,
    },

    /// Print shell completion scripts: nawa completions bash|zsh|fish
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Run one sandbox spec (internal; used by `nawa build`)
    #[command(hide = true)]
    SandboxInit {
        /// path to the spec.toml written by the build engine
        spec: PathBuf,
    },

    /// Report which sandbox features this kernel allows (internal/tests)
    #[command(hide = true)]
    SandboxProbe {
        /// run a single probe: userns | mounts | ro-bind | proc | pivot | netns-lo
        op: Option<String>,
    },

    /// List every known package name (recipes + installed) for shell
    /// completion — one bare name per line (internal, used by completions)
    #[command(name = "__complete_pkgs", hide = true)]
    CompletePkgs,
}

/// Flags shared by the commands that build and install by name.
#[derive(clap::Args, Clone)]
struct PlanFlags {
    /// Overwrite files owned by other packages
    #[arg(long)]
    force: bool,
    /// Accept packages without a signature (rejected when trust keys exist)
    #[arg(long)]
    allow_unsigned: bool,
    /// Do not ask for confirmation (non-interactive runs REQUIRE this)
    #[arg(short = 'y', long)]
    yes: bool,
    /// Show the plan and stop: nothing is built or installed
    #[arg(short = 'n', long)]
    dry_run: bool,
    /// Skip a build whose archive already exists with identical inputs
    /// (needs --isolation env or higher, see `nawa build --use-cache`)
    #[arg(long)]
    use_cache: bool,
    /// Isolation level for the builds: none | env | sandbox | closure
    #[arg(long)]
    isolation: Option<String>,
    /// Continue when the host kernel refuses an isolation feature
    #[arg(long)]
    allow_degraded: bool,
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// List services in /etc/sv and their enabled state
    List,
    /// Enable a service (link into the default runsvdir)
    Enable { name: String },
    /// Disable a service (remove the runsvdir link)
    Disable { name: String },
    /// Show service status (static state + live `sv status` when supervised)
    Status { name: Option<String> },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the effective configuration, annotated with each value's source
    Show,
}

#[derive(Clone, Copy, ValueEnum)]
enum Shell {
    Bash,
    Zsh,
    Fish,
}

struct Ctx {
    root: PathBuf,
    recipes: PathBuf,
    builddir: PathBuf,
    profiles: PathBuf,
    jobs: Option<u32>,
    repo: Option<String>,
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

/// Which database lock a command needs: writers exclusive, readers shared.
/// Taken once here (the library functions never lock — `upgrade` calls
/// `install_archive`, and flock would deadlock against itself).
fn lock_mode(cmd: &Cmd) -> Option<lock::Mode> {
    use lock::Mode::{Exclusive, Shared};
    match cmd {
        // installing archives holds the lock for the whole command; installing
        // by name builds first, so (like `build --install`) it locks around
        // each install step only
        Cmd::Install { targets, .. } => {
            if targets.iter().all(|t| is_archive_arg(t)) {
                Some(Exclusive)
            } else {
                None
            }
        }
        Cmd::Remove { .. } | Cmd::Rollback { .. } | Cmd::Autoremove { .. } => Some(Exclusive),
        Cmd::Upgrade { dry_run, .. } => Some(if *dry_run { Shared } else { Exclusive }),
        // `build --install` / `update` lock themselves around the install
        // step only (holding the db lock through a full build would starve
        // readers)
        Cmd::Build { .. } | Cmd::Update { .. } => None,
        Cmd::Outdated => Some(Shared),
        Cmd::Query { .. }
        | Cmd::Info { .. }
        | Cmd::Why { .. }
        | Cmd::Files { .. }
        | Cmd::Owner { .. }
        | Cmd::Deps { .. }
        | Cmd::DepsAll { .. } => Some(Shared),
        // repo/sign tooling and everything below touches no package database
        Cmd::RepoCreate { .. }
        | Cmd::Keygen { .. }
        | Cmd::Sign { .. }
        | Cmd::Verify { .. }
        | Cmd::Trust { .. }
        | Cmd::Search { .. }
        | Cmd::Completions { .. }
        | Cmd::Config { .. }
        | Cmd::Init { .. }
        | Cmd::Service { .. }
        | Cmd::CompletePkgs => None,
        Cmd::SandboxInit { .. } | Cmd::SandboxProbe { .. } => None,
    }
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
        Cmd::Install { targets, flags, no_deps, key } => {
            cmd_install(ctx, &targets, &flags, no_deps, key)
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
        Cmd::RepoCreate { dir, out, sign, key } => {
            // a relative --out lands inside the repo dir, not in the CWD
            let out = if out.is_absolute() { out } else { dir.join(out) };
            repo::create_index(&dir, &out)?;
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
                    "no repository given: pass --repo <dir-or-url>, set `repo` in nawa.toml, \
                     or export NAWA_REPO"
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
        Cmd::Init { force } => cmd_init(force),
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

#[allow(clippy::too_many_arguments)]
fn cmd_build(
    ctx: &Ctx,
    name: &str,
    check: build::CheckMode,
    isolation: Option<&str>,
    rebuild_check: bool,
    allow_network: bool,
    allow_degraded: bool,
    timeout: Option<u64>,
    use_cache: bool,
    install_after: bool,
    install_force: bool,
) -> Result<()> {
    let rec = recipe::load_by_name(&ctx.recipes, name)?;
    let opts = prepare_build(
        ctx,
        &rec,
        check,
        isolation,
        allow_network,
        allow_degraded,
        timeout,
        use_cache,
    )?;
    if rebuild_check {
        build::rebuild_check(&rec, &opts)?;
    }
    let archive = if install_after {
        // --rebuild-check already produced the archive; otherwise ALWAYS
        // build — an archive left in out/ by an earlier run carries the same
        // name-version-rel even when the recipe or patches changed since.
        let expected = opts.outdir.join(format!("{}.nawa.tar.zst", rec.id()));
        if rebuild_check && expected.exists() {
            expected
        } else {
            build::build(&rec, &opts)?.archive
        }
    } else {
        build::build(&rec, &opts)?;
        return Ok(());
    };

    // ---- --install --------------------------------------------------------
    let pal = Palette::auto(Stream::Stdout);
    println!(
        "nawa: installing {} into {}",
        pal.ver(&archive.display().to_string()),
        ctx.root.display()
    );
    // the library functions never lock; this command-level lock covers just
    // the install step (upgrade calls install_archive the same way)
    let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
    install::install_archive(&archive, &ctx.root, install_force, false, None, "explicit")?;
    Ok(())
}

/// `nawa install` takes archive paths *or* package names. Something that
/// ends in `.nawa.tar.zst` or contains a `/` is a file; a bare word is a name.
fn is_archive_arg(arg: &str) -> bool {
    arg.ends_with(".nawa.tar.zst") || arg.contains('/')
}

/// Everything `build` derives from the profile and flags: jobs, flags env,
/// isolation level, timeout — shared by `build`, `install <name>` and `update`.
#[allow(clippy::too_many_arguments)]
fn prepare_build(
    ctx: &Ctx,
    rec: &recipe::Recipe,
    check: build::CheckMode,
    isolation: Option<&str>,
    allow_network: bool,
    allow_degraded: bool,
    timeout: Option<u64>,
    use_cache: bool,
) -> Result<build::BuildOpts> {
    let arch = rec.arch();
    let profile = profile::Profile::load(&ctx.profiles, &arch)?;
    let env = profile.as_ref().map(|p| p.env()).unwrap_or_default();
    // precedence: --jobs > profile `jobs` > number of CPUs
    let jobs = ctx
        .jobs
        .or_else(|| profile.as_ref().and_then(|p| p.jobs))
        .unwrap_or_else(default_jobs);
    if !env.is_empty() {
        let flags: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        println!("nawa: profile {arch}: {}", flags.join(" "));
    }

    // isolation precedence: --isolation > profile [build] isolation > none
    let build_profile = std::fs::read_to_string(ctx.profiles.join(format!("{arch}.toml")))
        .ok()
        .and_then(|raw| profile::Profile::parse_build(&raw).ok())
        .unwrap_or_default();
    let isolation = match isolation {
        Some(s) => buildenv::Isolation::parse(s)?,
        None => match build_profile.isolation.as_deref() {
            Some(s) if !s.trim().is_empty() => buildenv::Isolation::parse(s)?,
            _ => buildenv::Isolation::None,
        },
    };
    let timeout_secs = timeout.or(build_profile.timeout).unwrap_or(0);

    let profile_flags = env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");

    Ok(build::BuildOpts {
        jobs,
        builddir: ctx.builddir.clone(),
        outdir: ctx.builddir.join("out"),
        env,
        check,
        isolation,
        timeout_secs,
        allow_network,
        allow_degraded,
        root: ctx.root.clone(),
        recipes_root: ctx.recipes.clone(),
        allow_unsigned: false,
        profile_flags,
        use_cache,
    })
}

fn cmd_install(
    ctx: &Ctx,
    targets: &[String],
    flags: &PlanFlags,
    no_deps: bool,
    key: Option<PathBuf>,
) -> Result<()> {
    let archives = targets.iter().filter(|t| is_archive_arg(t)).count();
    if archives == targets.len() {
        for t in targets {
            install_archive_file(ctx, Path::new(t), flags.force, flags.allow_unsigned, key.clone())?;
        }
        return Ok(());
    }
    if archives > 0 {
        bail!("do not mix package names and archive files in one `nawa install`");
    }
    if key.is_some() {
        bail!("--key only applies to archive files (package names are built locally)");
    }
    let known = recipe::load_all(&ctx.recipes)?;
    let installed = installed_map(ctx)?;
    let plan = plan::install_plan(targets, &installed, &known, plan::Why::Explicit, !no_deps)?;
    for name in &plan.up_to_date {
        println!("nawa: {name} is already installed and up to date");
    }
    run_plan(ctx, &plan, flags)
}

/// The installed packages by name, read under a brief shared lock.
fn installed_map(ctx: &Ctx) -> Result<std::collections::HashMap<String, db::Entry>> {
    let _lock = lock::acquire(&ctx.root, lock::Mode::Shared)?;
    Ok(db::Db::new(&ctx.root)
        .installed()?
        .into_iter()
        .map(|e| (e.name.clone(), e))
        .collect())
}

/// "3m05s" / "1h02m" / "12s"
fn format_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// Show the plan, ask, then build and install every item in order —
/// dependencies first, one `[n/total]` line per step.
fn run_plan(ctx: &Ctx, plan: &plan::Plan, flags: &PlanFlags) -> Result<()> {
    let total = plan.items.len();
    if total == 0 {
        println!("nawa: nothing to do");
        return Ok(());
    }
    let pal = Palette::auto(Stream::Stdout);
    println!(
        "nawa: {total} package(s) to build and install into {}:",
        ctx.root.display()
    );
    for (i, it) in plan.items.iter().enumerate() {
        let replaces = match &it.replaces {
            Some(old) => format!("  replaces {old}"),
            None => String::new(),
        };
        println!(
            "  {:>3}. {} {}  {}{}",
            i + 1,
            pal.name(&format!("{:<22}", it.rec.package.name)),
            pal.ver(&format!(
                "{:<12}",
                format!("{}-{}", it.rec.package.version, it.rec.package.rel)
            )),
            pal.dim(it.why.label()),
            pal.dim(&replaces),
        );
    }
    if flags.dry_run {
        println!("nawa: dry run — nothing was built or installed");
        return Ok(());
    }
    confirm::confirm(&format!("nawa: build and install {total} package(s)?"), flags.yes)?;

    let started = std::time::Instant::now();
    for (i, it) in plan.items.iter().enumerate() {
        let n = i + 1;
        let id = it.rec.id();
        let step = || -> Result<()> {
            println!("nawa: [{n}/{total}] building {id}");
            let opts = prepare_build(
                ctx,
                &it.rec,
                build::CheckMode::Off,
                flags.isolation.as_deref(),
                false,
                flags.allow_degraded,
                None,
                flags.use_cache,
            )?;
            let archive = build::build(&it.rec, &opts)
                .with_context(|| format!("[{n}/{total}] build of {id} failed"))?
                .archive;
            println!("nawa: [{n}/{total}] installing {id}");
            // the library functions never lock; hold the db lock for this one
            // install only, never through a build
            let _lock = lock::acquire(&ctx.root, lock::Mode::Exclusive)?;
            install::install_archive(
                &archive,
                &ctx.root,
                flags.force,
                flags.allow_unsigned,
                None,
                it.why.db_reason(),
            )
            .with_context(|| format!("[{n}/{total}] install of {id} failed"))?;
            Ok(())
        };
        if let Err(e) = step() {
            if i > 0 {
                eprintln!(
                    "nawa: {i} of {total} package(s) were installed before this failure — \
                     re-run the same command to continue (installed packages are skipped)"
                );
            }
            return Err(e);
        }
    }
    println!(
        "nawa: done — {total} package(s) installed in {}",
        format_duration(started.elapsed().as_secs())
    );
    Ok(())
}

fn cmd_outdated(ctx: &Ctx) -> Result<()> {
    let known = recipe::load_all(&ctx.recipes)?;
    let installed = db::Db::new(&ctx.root).installed()?;
    let list = plan::outdated(&installed, &known);
    if list.is_empty() {
        println!("nawa: everything is up to date ({} installed)", installed.len());
        return Ok(());
    }
    let pal = Palette::auto(Stream::Stdout);
    for (e, r) in &list {
        println!(
            "{} {} -> {}",
            pal.name(&format!("{:<24}", e.name)),
            pal.dim(&format!("{:<14}", format!("{}-{}", e.version, e.rel))),
            pal.ver(&format!("{}-{}", r.package.version, r.package.rel)),
        );
    }
    println!("nawa: {} package(s) can be updated — run `nawa update`", list.len());
    Ok(())
}

fn cmd_update(ctx: &Ctx, names: &[String], flags: &PlanFlags) -> Result<()> {
    let known = recipe::load_all(&ctx.recipes)?;
    let installed = installed_map(ctx)?;
    for n in names {
        if !installed.contains_key(n) {
            bail!("`{n}` is not installed (use `nawa install {n}`)");
        }
    }
    let list: Vec<db::Entry> = installed.values().cloned().collect();
    let targets: Vec<String> = plan::outdated(&list, &known)
        .into_iter()
        .map(|(e, _)| e.name.clone())
        .filter(|n| names.is_empty() || names.contains(n))
        .collect();
    if targets.is_empty() {
        println!("nawa: everything is up to date");
        return Ok(());
    }
    let plan = plan::install_plan(&targets, &installed, &known, plan::Why::Update, true)?;
    run_plan(ctx, &plan, flags)
}

fn cmd_autoremove(ctx: &Ctx, dry_run: bool, yes: bool, build_deps: bool) -> Result<()> {
    let known = recipe::load_all(&ctx.recipes)?;
    if known.is_empty() && !build_deps {
        bail!(
            "no recipes under {} — cannot tell which dependencies are build tools to keep \
             (point --recipes at the tree, or pass --build-deps to ignore build tools)",
            ctx.recipes.display()
        );
    }
    let installed = db::Db::new(&ctx.root).installed()?;
    let orphans = plan::orphans(&installed, &known, build_deps);
    if orphans.is_empty() {
        println!("nawa: nothing to remove");
        return Ok(());
    }
    let order = plan::removal_order(&orphans, &installed);
    let pal = Palette::auto(Stream::Stdout);
    println!("nawa: {} unneeded dependency package(s):", order.len());
    for n in &order {
        println!("  {}", pal.name(n));
    }
    if dry_run {
        println!("nawa: dry run — nothing was removed");
        return Ok(());
    }
    confirm::confirm(&format!("nawa: remove {} package(s)?", order.len()), yes)?;
    for (i, n) in order.iter().enumerate() {
        println!("nawa: [{}/{}] removing {n}", i + 1, order.len());
        install::remove_with(n, &ctx.root, false)?;
    }
    println!("nawa: done — {} package(s) removed", order.len());
    Ok(())
}

fn install_archive_file(
    ctx: &Ctx,
    file: &Path,
    force: bool,
    allow_unsigned: bool,
    key: Option<PathBuf>,
) -> Result<()> {
    if !file.exists() {
        // a near-miss hint saves a trip to `ls build/out`
        let dir = match file.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        let wanted = file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| file.display().to_string());
        let names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        bail!(
            "archive not found: {}{}",
            file.display(),
            suggest::hint(&wanted, &names)
        );
    }
    install::install_archive(file, &ctx.root, force, allow_unsigned, key.as_deref(), "explicit")?;
    Ok(())
}

fn cmd_search(ctx: &Ctx, term: &str, glob: bool) -> Result<()> {
    let all = recipe::load_all(&ctx.recipes)?;
    let db = db::Db::new(&ctx.root);
    let installed: HashSet<String> = db.installed()?.into_iter().map(|e| e.name).collect();
    let pal = Palette::auto(Stream::Stdout);
    let needle = term.to_lowercase();

    let mut hits = 0usize;
    for (name, rec) in &all {
        let nl = name.to_lowercase();
        let matched = if glob {
            glob_match(&needle, &nl)
        } else {
            nl.contains(&needle)
                || rec.package.summary.to_lowercase().contains(&needle)
        };
        if !matched {
            continue;
        }
        hits += 1;
        let mark = if installed.contains(name) {
            pal.ok("*")
        } else {
            " ".to_string()
        };
        println!(
            "{mark} {} {}  {}",
            pal.name(&format!("{nl:<24}")),
            pal.ver(&format!("{:<12}", format!("{}-{}", rec.package.version, rec.package.rel))),
            pal.dim(&rec.package.summary),
        );
    }
    if hits == 0 {
        eprintln!(
            "nawa: {} no recipes match {term:?} in {}",
            pal.warn("note:"),
            ctx.recipes.display()
        );
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_info(ctx: &Ctx, name: &str) -> Result<()> {
    let rec = recipe::load_by_name(&ctx.recipes, name)?;
    let db = db::Db::new(&ctx.root);
    let entry = db.entry(name)?;
    let pal = Palette::auto(Stream::Stdout);

    let row = |label: &str, value: String| {
        println!("{:<10} {}", pal.header(&format!("{label}:")), value);
    };

    let recipe_path = recipe::find_recipe_path(&ctx.recipes, name)?;
    row("name", pal.name(name));
    row("recipe", recipe_path.display().to_string());
    row(
        "version",
        pal.ver(&format!("{}-{}", rec.package.version, rec.package.rel)),
    );
    row("arch", rec.arch());
    if !rec.package.summary.is_empty() {
        row("summary", rec.package.summary.clone());
    }
    if !rec.package.license.is_empty() {
        row("license", rec.package.license.join(", "));
    }
    let src = if rec.package.source.is_empty() {
        "(meta package — no source)".to_string()
    } else if let Some(p) = rec.package.source.strip_prefix("local:") {
        format!("local:{p}")
    } else if rec.package.sha256.is_empty() {
        pal.warn(&format!("{} (source sha256 NOT pinned)", rec.package.source))
    } else {
        format!("{} (sha256 pinned)", rec.package.source)
    };
    row("source", src);

    let dep = |list: &[String]| {
        if list.is_empty() {
            "-".to_string()
        } else {
            list.join(", ")
        }
    };
    println!(
        "{:<10} {} build: {}  run: {}  check: {}",
        pal.header("depends:"),
        pal.dim("·"),
        pal.dim(&dep(&rec.depends.build)),
        pal.dim(&dep(&rec.depends.run)),
        pal.dim(&dep(&rec.depends.check)),
    );
    row(
        "sandbox",
        format!(
            "network={} sysfs={}",
            if rec.sandbox.network { "on" } else { "off" },
            if rec.sandbox.sysfs { "on" } else { "off" },
        ),
    );

    match entry {
        Some(e) => {
            let mut detail = format!(
                "{} ({}, installed {})",
                pal.ok(&e.version_full()),
                e.reason,
                format_date(e.install_date),
            );
            if e.size > 0 {
                detail.push_str(&format!(", {}", format_size(e.size)));
            }
            let nfiles = db.files(name)?.len();
            if nfiles > 0 {
                detail.push_str(&format!(", {nfiles} files"));
            }
            row("installed", detail);
        }
        None => row("installed", pal.dim("not installed")),
    }
    Ok(())
}

fn cmd_why(ctx: &Ctx, name: &str, tree: bool) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    let pal = Palette::auto(Stream::Stdout);
    let entry = db.entry(name)?;
    let dependents: Vec<String> = db
        .installed()?
        .into_iter()
        .filter(|e| e.run.iter().any(|r| r == name))
        .map(|e| e.name)
        .collect();

    match &entry {
        Some(e) => {
            let reason = if e.reason == "explicit" {
                pal.ok(&e.reason)
            } else {
                pal.dim(&e.reason)
            };
            println!(
                "{} — {} ({})",
                pal.name(name),
                pal.ver(&e.version_full()),
                reason
            );
        }
        None => println!("{} — {}", pal.name(name), pal.dim("not installed")),
    }

    if dependents.is_empty() {
        println!("  nothing installed depends on it");
    } else {
        println!("  required by {} installed package(s):", dependents.len());
        for d in &dependents {
            println!("    {}", pal.name(d));
        }
    }

    if tree {
        let all = recipe::load_all(&ctx.recipes)?;
        let mut rdeps: Vec<(String, Vec<&str>)> = Vec::new();
        for (n, r) in &all {
            if n == name {
                continue;
            }
            let mut kinds: Vec<&str> = Vec::new();
            if r.depends.build.iter().any(|d| d == name) {
                kinds.push("build");
            }
            if r.depends.run.iter().any(|d| d == name) {
                kinds.push("run");
            }
            if !kinds.is_empty() {
                rdeps.push((n.clone(), kinds));
            }
        }
        if rdeps.is_empty() {
            println!("  no recipe in {} depends on it", ctx.recipes.display());
        } else {
            println!(
                "  required by {} recipe(s) in {}:",
                rdeps.len(),
                ctx.recipes.display()
            );
            for (n, kinds) in &rdeps {
                println!("    {} ({})", pal.name(n), pal.dim(&kinds.join(", ")));
            }
        }
    }

    if entry.is_none() && dependents.is_empty() {
        bail!("`{name}` is not installed and nothing depends on it");
    }
    Ok(())
}

fn cmd_deps(ctx: &Ctx, name: &str) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    let installed: HashSet<String> =
        db.installed()?.into_iter().map(|e| e.name).collect();
    let order = deps::resolve_from_recipes(&ctx.recipes, &[name.to_string()], &installed)?;
    let pal = Palette::auto(Stream::Stdout);
    for (i, r) in order.iter().enumerate() {
        println!(
            "{:>3}. {} (build deps: {})",
            i + 1,
            pal.ver(&r.id()),
            pal.dim(&if r.depends.build.is_empty() {
                "-".into()
            } else {
                r.depends.build.join(", ")
            })
        );
    }
    Ok(())
}

fn cmd_deps_all(ctx: &Ctx, include_installed: bool) -> Result<()> {
    let all = recipe::load_all(&ctx.recipes)?;
    if all.is_empty() {
        bail!("no recipes found under {}", ctx.recipes.display());
    }
    let db = db::Db::new(&ctx.root);
    // default: skip what the db already records (a resumed stage2 rebuilds
    // only what is missing). --include-installed ignores the db entirely so
    // a stage3 world rebuild actually rebuilds everything — the resolver
    // treats "installed" as "done", which would otherwise empty the order.
    let installed: HashSet<String> = if include_installed {
        HashSet::new()
    } else {
        db.installed()?.into_iter().map(|e| e.name).collect()
    };
    let targets: Vec<String> = all.keys().cloned().collect();
    let order = deps::resolve(&targets, &installed, &all)?;
    for r in order {
        println!("{}", r.package.name);
    }
    Ok(())
}

fn cmd_remove(ctx: &Ctx, name: &str, force: bool, yes: bool) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    let entry = match db.entry(name)? {
        Some(e) => e,
        None => bail!("`{name}` is not installed"),
    };
    let dependents: Vec<String> = db
        .installed()?
        .into_iter()
        .filter(|e| e.name != name && e.run.iter().any(|r| r == name))
        .map(|e| e.name)
        .collect();
    let nfiles = db.files(name)?.len();

    // the confirmation shows what will actually happen; the removal itself
    // re-checks dependents (refusing without --force) after the prompt
    let pal = Palette::auto(Stream::Stdout);
    println!("nawa: {} {} ({} file(s))", pal.warn("removing"), pal.name(name), nfiles);
    println!("     {} {} — {}", pal.dim("version"), pal.ver(&entry.version_full()), pal.dim(&format_date(entry.install_date)));
    if !dependents.is_empty() {
        println!(
            "     {} {}",
            pal.warn("required by:"),
            pal.name(&dependents.join(", "))
        );
        if !force {
            println!(
                "     {}",
                pal.warn("removal will fail unless --force is passed")
            );
        }
    }
    confirm::confirm(
        &format!("nawa: remove {name} from {}?", ctx.root.display()),
        yes,
    )?;
    install::remove_with(name, &ctx.root, force)
}

fn cmd_query(ctx: &Ctx, pattern: Option<&str>, explicit: bool, deps: bool) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    let mut pkgs = db.installed()?;
    if explicit {
        pkgs.retain(|p| p.reason != "dependency");
    } else if deps {
        pkgs.retain(|p| p.reason == "dependency");
    }
    let pal = Palette::auto(Stream::Stdout);
    if pkgs.is_empty() {
        println!("nawa: no packages installed in {}", ctx.root.display());
        return Ok(());
    }
    let needle = pattern.map(|p| p.to_lowercase());
    if pal.enabled() {
        println!(
            "{} {} {} {}",
            pal.header(&format!("{:<24}", "PACKAGE")),
            pal.header(&format!("{:<12}", "VERSION")),
            pal.header(&format!("{:<6}", "REL")),
            pal.header("REASON"),
        );
    }
    let mut shown = 0usize;
    for p in pkgs {
        if let Some(n) = &needle {
            if !p.name.to_lowercase().contains(n) {
                continue;
            }
        }
        shown += 1;
        let reason = if p.reason == "explicit" {
            pal.ok(&p.reason)
        } else {
            pal.dim(&p.reason)
        };
        if pal.enabled() {
            println!(
                "{} {} {} {}",
                pal.name(&format!("{:<24}", p.name)),
                pal.ver(&format!("{:<12}", p.version)),
                pal.dim(&format!("rel-{:<3}", p.rel)),
                reason,
            );
        } else {
            println!("{:<24} {:<12} rel-{:<3} {}", p.name, p.version, p.rel, p.reason);
        }
    }
    if shown == 0 {
        eprintln!(
            "nawa: no installed package matches {:?}",
            pattern.unwrap_or_default()
        );
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_files(ctx: &Ctx, name: &str) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    if db.entry(name)?.is_none() {
        bail!("`{name}` is not installed");
    }
    for f in db.files(name)? {
        println!("{f}");
    }
    Ok(())
}

fn cmd_owner(ctx: &Ctx, file: &str) -> Result<()> {
    let db = db::Db::new(&ctx.root);
    let owners = db.file_owners()?;
    match owners.get(file) {
        Some(owner) => println!("{file} is owned by {owner}"),
        None => bail!("no installed package owns {file}"),
    }
    Ok(())
}

fn cmd_config_show(ctx: &Ctx) -> Result<()> {
    // the config resolved at startup, flags included, so sources are truthful
    let cfg = &ctx.cfg;
    let pal = Palette::auto(Stream::Stdout);
    println!("{} {}", pal.header("nawa config"), pal.dim("(precedence: flag > env > nawa.toml > default)"));
    let show = |key: &str, value: String, source: &str| {
        println!("{:<10} {:<40} {}", pal.header(key), value, pal.dim(&format!("({source})")));
    };
    show("root", ctx.root.display().to_string(), cfg.source_of("root"));
    show("recipes", ctx.recipes.display().to_string(), cfg.source_of("recipes"));
    show("builddir", ctx.builddir.display().to_string(), cfg.source_of("builddir"));
    show("profiles", ctx.profiles.display().to_string(), cfg.source_of("profiles"));
    let jobs = ctx
        .jobs
        .map(|j| j.to_string())
        .unwrap_or_else(|| "(profile `jobs`, else CPU count)".into());
    show("jobs", jobs, cfg.source_of("jobs"));
    show(
        "repo",
        ctx.repo.clone().unwrap_or_else(|| "(not set — pass --repo to upgrade)".into()),
        cfg.source_of("repo"),
    );
    if let Some(f) = &cfg.file {
        println!("{:<10} {}", pal.header("config"), f.display());
    } else {
        println!("{:<10} {}", pal.header("config"), pal.dim("(none found — run `nawa init` to create one)"));
    }
    Ok(())
}

fn cmd_init(force: bool) -> Result<()> {
    let path = PathBuf::from(config::CONFIG_FILE);
    if path.exists() && !force {
        bail!(
            "{} already exists here (use --force to overwrite)",
            path.display()
        );
    }
    std::fs::write(&path, config::template())
        .with_context(|| format!("write {}", path.display()))?;
    println!(
        "nawa: wrote {} — uncomment what you need, then check with `nawa config show`",
        path.display()
    );
    Ok(())
}

/// Every known package name: the recipes tree plus the local db. One bare
/// name per line — the completion scripts call this dynamically.
fn cmd_complete_pkgs(ctx: &Ctx) -> Result<()> {
    let mut names: BTreeSet<String> = recipe::load_all(&ctx.recipes)?
        .into_keys()
        .collect();
    for e in db::Db::new(&ctx.root).installed()? {
        names.insert(e.name);
    }
    for n in names {
        println!("{n}");
    }
    Ok(())
}

// ---- presentation helpers (unit-tested below) ------------------------------

/// Case-insensitive glob with `*` (any run) and `?` (any single char),
/// anchored at both ends — `nawa search --glob`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    fn rec(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some('*'), _) => rec(&p[1..], t) || (!t.is_empty() && rec(p, &t[1..])),
            (Some('?'), Some(_)) => rec(&p[1..], &t[1..]),
            (Some(&c), Some(&x)) if c == x => rec(&p[1..], &t[1..]),
            _ => false,
        }
    }
    rec(&p, &t)
}

/// Unix seconds -> "YYYY-MM-DD" (UTC). 0 (never recorded) renders as "-".
fn format_date(secs: u64) -> String {
    if secs == 0 {
        return "-".to_string();
    }
    // Howard Hinnant's civil_from_days: valid for the whole u64 seconds range
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Byte count in human units.
fn format_size(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if bytes >= 1 << 30 {
        format!("{:.1} GiB", b / (K * K * K))
    } else if bytes >= 1 << 20 {
        format!("{:.1} MiB", b / (K * K))
    } else if bytes >= 1 << 10 {
        format!("{:.1} KiB", b / K)
    } else {
        format!("{bytes} B")
    }
}

// ---- shell completions (hand-written; no extra crates) ---------------------
// Package names complete dynamically via `nawa __complete_pkgs`.

const COMPLETION_BASH: &str = r#"# bash completion for nawa (generated by `nawa completions bash`)
_nawa() {
    local cur prev commands pkgs
    cur="${COMP_WORDS[COMP_CWORD]}"
    commands="build install outdated update autoremove deps deps-all search info why query list remove files owner repo-create keygen sign verify trust upgrade rollback service config init completions"
    pkgs="$(nawa __complete_pkgs 2>/dev/null)"
    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "$commands" -- "$cur") )
        return 0
    fi
    case "${COMP_WORDS[1]}" in
        build|deps|deps-all|info|files|remove|rollback|why|update)
            COMPREPLY=( $(compgen -W "$pkgs" -- "$cur") ) ;;
        install)
            COMPREPLY=( $(compgen -W "$pkgs" -- "$cur") $(compgen -f -- "$cur") ) ;;
        sign|verify|trust|repo-create)
            COMPREPLY=( $(compgen -f -- "$cur") ) ;;
        service)
            if [ "$COMP_CWORD" -eq 2 ]; then
                COMPREPLY=( $(compgen -W "list enable disable status" -- "$cur") )
            else
                COMPREPLY=( $(compgen -W "$(nawa service list 2>/dev/null | awk '{print $1}' | grep -v '^SERVICE$')" -- "$cur") )
            fi ;;
        config)
            COMPREPLY=( $(compgen -W "show" -- "$cur") ) ;;
        completions)
            COMPREPLY=( $(compgen -W "bash zsh fish" -- "$cur") ) ;;
        *)
            COMPREPLY=( $(compgen -f -- "$cur") ) ;;
    esac
    return 0
}
complete -F _nawa nawa
"#;

const COMPLETION_ZSH: &str = r#"#compdef nawa
# zsh completion for nawa (generated by `nawa completions zsh`)
_nawa() {
    local -a cmds
    cmds=(
        'build:Build a package from its recipe'
        'install:Install packages by name (with dependencies) or from archives'
        'outdated:List installed packages with a newer recipe'
        'update:Rebuild and reinstall outdated packages'
        'autoremove:Remove dependencies nothing needs any more'
        'search:Search the recipes tree by name or summary'
        'info:Show everything known about a package'
        'why:Show which packages depend on a package'
        'deps:Print the dependency resolution order'
        'deps-all:Print the global build order for all recipes'
        'query:List installed packages (alias: list)'
        'list:List installed packages'
        'remove:Remove an installed package'
        'files:List files owned by an installed package'
        'owner:Find which package owns a file'
        'repo-create:Create a repository index'
        'keygen:Generate an ed25519 signing keypair'
        'sign:Sign a package archive or index'
        'verify:Verify a signature'
        'trust:Add a public key to the trust store'
        'upgrade:Upgrade installed packages from a repository'
        'rollback:Roll a package back to the previous version'
        'service:Manage runit services'
        'config:Show the effective configuration'
        'init:Write a nawa.toml template'
        'completions:Print shell completion scripts'
    )
    local -a pkgs
    pkgs=(${(f)"$(nawa __complete_pkgs 2>/dev/null)"})
    if (( CURRENT == 2 )); then
        _describe -t commands 'nawa command' cmds
    else
        case $words[2] in
            build|deps|deps-all|info|files|remove|rollback|why|query|list|update)
                _describe -t packages 'package' pkgs ;;
            install)
                _describe -t packages 'package' pkgs
                _files ;;
            service)
                if (( CURRENT == 3 )); then
                    _values 'action' list enable disable status
                else
                    _describe -t services 'service' \
                        services=($(nawa service list 2>/dev/null | awk 'NR>1{print $1}'))
                fi ;;
            config)
                _values 'action' show ;;
            completions)
                _values 'shell' bash zsh fish ;;
        esac
    fi
}
_nawa "$@"
"#;

const COMPLETION_FISH: &str = r#"# fish completion for nawa (generated by `nawa completions fish`)
complete -c nawa -f
complete -c nawa -n '__fish_use_subcommand' -a 'build' -d 'Build a package from its recipe'
complete -c nawa -n '__fish_use_subcommand' -a 'install' -d 'Install packages by name or archive'
complete -c nawa -n '__fish_use_subcommand' -a 'outdated' -d 'List packages with a newer recipe'
complete -c nawa -n '__fish_use_subcommand' -a 'update' -d 'Rebuild and reinstall outdated packages'
complete -c nawa -n '__fish_use_subcommand' -a 'autoremove' -d 'Remove unneeded dependencies'
complete -c nawa -n '__fish_use_subcommand' -a 'search' -d 'Search the recipes tree'
complete -c nawa -n '__fish_use_subcommand' -a 'info' -d 'Show package details'
complete -c nawa -n '__fish_use_subcommand' -a 'why' -d 'Show reverse dependencies'
complete -c nawa -n '__fish_use_subcommand' -a 'deps' -d 'Dependency resolution order'
complete -c nawa -n '__fish_use_subcommand' -a 'deps-all' -d 'Global build order'
complete -c nawa -n '__fish_use_subcommand' -a 'query list' -d 'List installed packages'
complete -c nawa -n '__fish_use_subcommand' -a 'remove' -d 'Remove an installed package'
complete -c nawa -n '__fish_use_subcommand' -a 'files owner repo-create keygen sign verify trust upgrade rollback service config init completions'
# package names, completed dynamically
set -l pkg_cmds build install update deps deps-all info files remove rollback why query list
complete -c nawa -n "__fish_seen_subcommand_from $pkg_cmds" -a "(nawa __complete_pkgs 2>/dev/null)"
# service actions
complete -c nawa -n '__fish_seen_subcommand_from service; and not __fish_seen_subcommand_from list enable disable status' -a 'list enable disable status'
# config actions
complete -c nawa -n '__fish_seen_subcommand_from config' -a 'show'
# completion shells
complete -c nawa -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish'
"#;

fn completions_script(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => COMPLETION_BASH,
        Shell::Zsh => COMPLETION_ZSH,
        Shell::Fish => COMPLETION_FISH,
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

