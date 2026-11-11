//! Command-line definition: clap types, shell completions and the lock
//! policy per command. Pure move out of `main.rs`; no behaviour change.

use crate::*;
use clap::{Parser, Subcommand, ValueEnum};
use std::ffi::OsString;

#[derive(Parser)]
#[command(
    name = "nawa",
    version,
    about = "nawa — source-based package manager for a from-scratch Linux distribution",
    after_help = "Global flags (--root --recipes --builddir --profiles --jobs) work \
                  anywhere on the line and may come from $NAWA_* variables or a nawa.toml \
                  config file (see `nawa init`, inspect with `nawa config show`).\n\
                  System-wide defaults (repository, install mode, ...) live in \
                  /etc/nawa/nawa.toml.

Examples:
  nawa search zip                        find recipes matching \"zip\"
  nawa info zstd                         full package details
  nawa install curl                      install curl (+ missing deps): from the
                                        repository when it has them, else build
  nawa install --build curl              always build from the recipes
  nawa install --from-repo curl          repository only (error if not there)
  nawa install -n curl                   show that plan, change nothing
  nawa outdated / nawa update             see and apply newer recipe versions
  nawa update --from repo                use the configured binary repository
  nawa update --from auto                prefer repo, build when it is not current
  nawa autoremove                        drop dependencies nothing needs
  nawa build --install curl              build and install ONE recipe
  nawa install --root /mnt/root a.nawa.tar.zst
  nawa why --tree curl                   who needs curl (installed + recipe tree)"
)]
pub(crate) struct Cli {
    /// Target root filesystem (use a throwaway dir for testing)
    #[arg(long, global = true)]
    pub(crate) root: Option<PathBuf>,

    /// Root of the recipes tree
    #[arg(long, global = true)]
    pub(crate) recipes: Option<PathBuf>,

    /// Scratch/build directory
    #[arg(long, global = true)]
    pub(crate) builddir: Option<PathBuf>,

    /// Directory with per-architecture build profiles (`<arch>.toml`)
    #[arg(long, global = true)]
    pub(crate) profiles: Option<PathBuf>,

    /// Parallel build jobs (default: the profile's `jobs`, else the CPU count)
    #[arg(long, global = true)]
    pub(crate) jobs: Option<u32>,

    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

pub(crate) fn default_jobs() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
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
        /// Refuse every download: only entries `nawa fetch` (or an earlier
        /// online build) populated may be used — the CI gate of §15
        #[arg(long)]
        offline: bool,
        /// Install the freshly built archive into --root when the build
        /// succeeds (replaces the manual find-the-archive + install dance)
        #[arg(long)]
        install: bool,
        /// With --install: overwrite files owned by other packages
        #[arg(long, requires = "install")]
        force: bool,
    },

    /// Install packages by name — from the binary repository when it has
    /// them, otherwise built from the recipes (dependencies included, after
    /// showing the plan) — or install built .nawa.tar.zst archives
    Install {
        /// package names (`nawa install curl`) or archive paths
        /// (`nawa install build/out/curl-8.9-1.nawa.tar.zst`)
        #[arg(required = true)]
        targets: Vec<String>,
        #[command(flatten)]
        flags: PlanFlags,
        /// By name: install only the named packages, skip their dependencies
        #[arg(long)]
        no_deps: bool,
        /// Archives only: verify the signature against this public key
        /// instead of the trust store
        #[arg(long)]
        key: Option<PathBuf>,
        /// Always build from the recipes (same as `--from build`)
        #[arg(short = 'b', long, conflicts_with_all = ["from_repo", "from"])]
        build: bool,
        /// Install from the repository only; a package it lacks is an error
        /// (same as `--from repo`)
        #[arg(short = 'r', long = "from-repo", conflicts_with = "from")]
        from_repo: bool,
        /// Where packages come from: auto | repo | build | ask
        /// (default: `install_mode` in nawa.toml, else auto)
        #[arg(long, value_name = "MODE")]
        from: Option<String>,
        /// Repository directory or http(s) URL for this run
        /// (default: the `repo` key in nawa.toml / $NAWA_REPO / built-in)
        #[arg(long, value_name = "DIR_OR_URL")]
        repo: Option<String>,
    },

    /// List installed packages whose recipe has a newer version
    Outdated,

    /// Update installed packages whose local recipes are newer; defaults to building
    /// from source, with an optional repository source
    Update {
        /// only update these packages (default: every outdated one)
        names: Vec<String>,
        #[command(flatten)]
        flags: PlanFlags,
        /// Always build from the recipes (the default)
        #[arg(short = 'b', long, conflicts_with_all = ["from_repo", "from"])]
        build: bool,
        /// Use only the binary repository; fail if it cannot provide an update
        #[arg(short = 'r', long = "from-repo", conflicts_with = "from")]
        from_repo: bool,
        /// Package source: build | repo | auto | ask
        #[arg(long, value_name = "MODE")]
        from: Option<String>,
        /// Repository directory or http(s) URL for this run
        #[arg(long, value_name = "DIR_OR_URL")]
        repo: Option<String>,
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

    /// Print the dependency resolution order for a package: what is not
    /// installed yet, dependencies first, the package itself last. The
    /// package is walked even when it is installed (a rebuild of glibc still
    /// needs its build tools).
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

    /// Check installed ELF files for broken shared-library links
    /// (missing libraries / interpreter) and list the packages to rebuild
    Revdep {
        /// only these packages (default: every installed package)
        names: Vec<String>,
        /// extra library directory, searched before the system ones
        #[arg(long = "lib-dir", value_name = "DIR")]
        lib_dirs: Vec<String>,
        /// do not report missing libraries matching this glob (repeatable)
        #[arg(long, value_name = "GLOB")]
        ignore: Vec<String>,
        /// do not report broken symlinks
        #[arg(long)]
        no_symlinks: bool,
        /// also scan files that no package owns (/usr /bin /sbin /lib /lib64 /opt)
        #[arg(long)]
        unowned: bool,
        /// scan this directory for unowned files (repeatable, implies --unowned)
        #[arg(long = "scan-dir", value_name = "DIR")]
        scan_dirs: Vec<String>,
        /// machine-readable output: one JSON document on stdout
        #[arg(long)]
        json: bool,
        /// do not look for the packages that provide missing libraries
        #[arg(long)]
        no_suggest: bool,
    },

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
        /// Repository index schema (1 = legacy, 2 = signed-index hashes, 3 = distributed per-archive HTTPS URLs)
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=3))]
        index_version: u32,
        /// TOML manifest containing package_urls and recipe_urls (required for schema 3)
        #[arg(long)]
        locations: Option<PathBuf>,
        /// Also pack this recipe tree into the repository (for `nawa sync`)
        #[arg(long, requires = "recipes_rev")]
        recipes: Option<PathBuf>,
        /// Revision recorded for --recipes (commit sha or tag)
        #[arg(long, requires = "recipes")]
        recipes_rev: Option<String>,
        /// Publish this validated group catalog as signed index metadata
        #[arg(long, requires = "sign")]
        groups: Option<PathBuf>,
    },

    /// Update a signed repository index incrementally; no archive is opened
    #[command(name = "repo-update")]
    RepoUpdate {
        /// directory that receives the recipe snapshot (--recipes) and holds a relative --out
        dir: PathBuf,
        /// previous index (index.toml); its signature must verify with --trust-key
        #[arg(long)]
        base: PathBuf,
        /// public key that must have signed --base; the update refuses otherwise
        #[arg(long)]
        trust_key: PathBuf,
        #[arg(long, default_value = "index.toml")]
        out: PathBuf,
        /// TOML file with [[package]] entries for the new archives
        #[arg(long)]
        add_entries: Option<PathBuf>,
        /// archive file that left the repository (repeatable)
        #[arg(long = "remove", value_name = "FILE")]
        remove: Vec<String>,
        /// package whose entry is dropped on purpose (repeatable)
        #[arg(long = "drop", value_name = "NAME")]
        drop: Vec<String>,
        /// TOML manifest with package_urls and recipe_urls (schema-3 base index)
        #[arg(long)]
        locations: Option<PathBuf>,
        /// Also pack this recipe tree into the repository (for `nawa sync`)
        #[arg(long, requires = "recipes_rev")]
        recipes: Option<PathBuf>,
        /// Revision recorded for --recipes (commit sha or tag)
        #[arg(long, requires = "recipes")]
        recipes_rev: Option<String>,
        /// Signing key; the updated index is always signed
        #[arg(long)]
        key: Option<PathBuf>,
    },

    /// Fetch the newest recipe tree from the repository (signature-checked)
    Sync {
        /// Repository (dir or URL); default: `repo` from the config
        #[arg(long)]
        repo: Option<String>,
        /// Accept an expired repository index
        #[arg(long)]
        force: bool,
        /// Accept an unsigned index (only when no trusted keys are configured)
        #[arg(long)]
        allow_unsigned: bool,
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
        /// (default: the `repo` key in nawa.toml / $NAWA_REPO / built-in)
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

    /// List configured repositories and their state (no downloads)
    Repolist {
        /// include disabled repositories
        #[arg(long)]
        all: bool,
    },

    /// Refresh repository metadata caches (signature + freshness checked)
    Makecache {
        /// accept unsigned indexes when no trusted keys exist
        #[arg(long)]
        allow_unsigned: bool,
        /// accept an expired repository index
        #[arg(long)]
        force: bool,
    },

    /// Download and verify external sources, then run the ecosystem
    /// fetch backends ([fetch.go] / [fetch.cargo]) — the pre-seed step so
    /// that later `--offline` builds have everything ready (§15)
    Fetch {
        /// package names
        names: Vec<String>,
        /// every recipe with a remote source or a fetch table (§15);
        /// legacy recipes included
        #[arg(long)]
        all: bool,
        /// Isolation level for the fetch runs: none | env | sandbox | closure
        /// (the same levels as build; the toolchain lives at closure)
        #[arg(long)]
        isolation: Option<String>,
        /// skip the ecosystem step: download and verify sources only
        /// (needs no toolchain; the CI pre-seed mode)
        #[arg(long)]
        sources_only: bool,
        /// verify-only: validate that every cached artifact and cache
        /// entry exists and is complete; downloads are refused and a
        /// missing entry is a hard error naming it (§15)
        #[arg(long)]
        offline: bool,
        /// Continue when the host kernel refuses an isolation feature
        #[arg(long)]
        allow_degraded: bool,
    },

    /// Inspect and check recipes (local files only, no network)
    Recipe {
        #[command(subcommand)]
        action: RecipeCmd,
    },

    /// Query repository metadata without downloading any archive
    Repoquery {
        name: String,
        /// restrict the query to this repository id
        #[arg(long)]
        repo_id: Option<String>,
        /// full package info instead of a one-line result
        #[arg(long)]
        info: bool,
    },

    /// Garbage-collect caches, keeping everything referenced
    Clean {
        category: String,
        /// show what would be removed, remove nothing
        #[arg(short = 'n', long)]
        dry_run: bool,
    },

    /// Install/remove groups of packages (metadata resolves to names)
    Group {
        #[command(subcommand)]
        action: GroupCmd,
    },

    /// Manage and run plugins (list / info / enable / disable / run)
    Plugin {
        #[command(subcommand)]
        action: PluginCmd,
    },

    /// Show the package transaction history (newest first)
    History {
        #[command(subcommand)]
        action: Option<HistoryCmd>,
        /// show at most this many transactions (default 20)
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Manage runit services (enable/disable/start/stop/restart/list/status)
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
    /// (or, with --system, the system-wide /etc/nawa/nawa.toml)
    Init {
        /// overwrite an existing nawa.toml
        #[arg(long)]
        force: bool,
        /// write /etc/nawa/nawa.toml (repository + install mode) instead
        #[arg(long)]
        system: bool,
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

    /// Unknown commands are offered to explicitly registered plugins.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

/// Flags shared by the commands that build and install by name.
#[derive(clap::Args, Clone)]
pub(crate) struct PlanFlags {
    /// Overwrite files owned by other packages
    #[arg(long)]
    pub(crate) force: bool,
    /// Accept packages without a signature (rejected when trust keys exist)
    #[arg(long)]
    pub(crate) allow_unsigned: bool,
    /// Do not ask for confirmation (non-interactive runs REQUIRE this)
    #[arg(short = 'y', long)]
    pub(crate) yes: bool,
    /// Show the plan and stop: nothing is built or installed
    #[arg(short = 'n', long)]
    pub(crate) dry_run: bool,
    /// Skip a build whose archive already exists with identical inputs
    /// (needs --isolation env or higher, see `nawa build --use-cache`)
    #[arg(long)]
    pub(crate) use_cache: bool,
    /// Isolation level for the builds: none | env | sandbox | closure
    #[arg(long)]
    pub(crate) isolation: Option<String>,
    /// Continue when the host kernel refuses an isolation feature
    #[arg(long)]
    pub(crate) allow_degraded: bool,
    /// Refuse every download: only cached, verified artifacts may be used
    /// (`nawa fetch` populates them); also settable through `NAWA_OFFLINE`
    #[arg(long)]
    pub(crate) offline: bool,
}

#[derive(Subcommand)]
pub(crate) enum RecipeCmd {
    /// Run the source-independent checks of §14B (warnings only): assets,
    /// unknown tables and keys, unknown placeholders, fetch anti-patterns
    Check {
        /// one recipe name (exclusive with --all)
        name: Option<String>,
        /// every recipe in the recipes tree
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum GroupCmd {
    /// List installed groups
    List,
    /// Show a group's recorded membership
    Info { id: String },
    /// Install a group's packages through the normal planner
    Install {
        id: String,
        /// Include optional packages listed by the group definition
        #[arg(long)]
        with_optional: bool,
        #[command(flatten)]
        flags: PlanFlags,
    },
    /// Remove a group's membership and its now-unneeded packages
    Remove {
        id: String,
        /// do not ask for confirmation (non-interactive runs REQUIRE this)
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum PluginCmd {
    /// List discovered plugins and their state
    List,
    /// Show one plugin's manifest details
    Info { id: String },
    /// Enable a plugin (stored in the [plugins] table of nawa.toml)
    Enable { id: String },
    /// Disable a plugin
    Disable { id: String },
    /// Grant a requested capability using system administrator policy
    Grant { id: String, capability: String },
    /// Revoke an administrator capability grant
    Revoke { id: String, capability: String },
    /// Run one of a plugin's commands with the given arguments
    Run {
        id: String,
        command: String,
        /// arguments passed through to the plugin (argv-preserving)
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum HistoryCmd {
    /// List transactions (the default `nawa history` view)
    List,
    /// Show the full story of one transaction
    Info { id: u64 },
    /// Compensate a previous transaction with a new one
    Undo {
        id: u64,
        /// do not ask for confirmation (non-interactive runs REQUIRE this)
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ServiceCmd {    /// List services in /etc/sv and their enabled state
    List,
    /// Enable a service (link into the default runsvdir); --now also starts it
    Enable {
        name: String,
        /// start it immediately and wait until it is running
        #[arg(long)]
        now: bool,
    },
    /// Disable a service (remove the runsvdir link); --now stops it first
    Disable {
        name: String,
        /// stop it first and wait until it is down
        #[arg(long)]
        now: bool,
    },
    /// Start an enabled service now
    Start { name: String },
    /// Stop a service now (stays enabled: it starts again at next boot)
    Stop { name: String },
    /// Restart an enabled service
    Restart { name: String },
    /// Show service status (static state + live `sv status` when supervised)
    Status { name: Option<String> },
}

#[derive(Subcommand)]
pub(crate) enum ConfigCmd {
    /// Print the effective configuration, annotated with each value's source
    Show,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum Shell {
    Bash,
    Zsh,
    Fish,
}

/// Which database lock a command needs: writers exclusive, readers shared.
/// Taken once here (the library functions never lock — `upgrade` calls
/// `install_archive`, and flock would deadlock against itself).
pub(crate) fn lock_mode(cmd: &Cmd) -> Option<lock::Mode> {
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
        Cmd::Remove { .. } | Cmd::Rollback { .. } | Cmd::Autoremove { .. } | Cmd::History { .. } => Some(Exclusive),
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
        | Cmd::DepsAll { .. } => Some(Shared),        // repo/sign tooling and everything below touches no package database
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
        Cmd::Repolist { .. } | Cmd::Repoquery { .. } | Cmd::Makecache { .. } | Cmd::Clean { .. }
        | Cmd::Plugin { .. } | Cmd::SandboxInit { .. } | Cmd::SandboxProbe { .. }
        | Cmd::External(_) => None,
    }
}

pub(crate) const COMPLETION_BASH: &str = r#"# bash completion for nawa (generated by `nawa completions bash`)
_nawa() {
    local cur prev commands pkgs
    cur="${COMP_WORDS[COMP_CWORD]}"
    commands="build install outdated update autoremove deps deps-all search info why query list remove files owner revdep repo-create repo-update sync keygen sign verify trust upgrade rollback repolist makecache repoquery fetch recipe clean group history plugin service config init completions"
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
        sign|verify|trust|repo-create|repo-update)
            COMPREPLY=( $(compgen -f -- "$cur") ) ;;
        history)
            if [ "$COMP_CWORD" -eq 2 ]; then
                COMPREPLY=( $(compgen -W "list info undo" -- "$cur") )
            fi ;;
        plugin)
            if [ "$COMP_CWORD" -eq 2 ]; then
                COMPREPLY=( $(compgen -W "list info enable disable grant revoke run" -- "$cur") )
            fi ;;
        group)
            if [ "$COMP_CWORD" -eq 2 ]; then
                COMPREPLY=( $(compgen -W "list info install remove" -- "$cur") )
            fi ;;
        service)
            if [ "$COMP_CWORD" -eq 2 ]; then
                COMPREPLY=( $(compgen -W "list enable disable start stop restart status" -- "$cur") )
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

pub(crate) const COMPLETION_ZSH: &str = r#"#compdef nawa
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
        'repo-update:Update a repository index incrementally'
        'keygen:Generate an ed25519 signing keypair'
        'sign:Sign a package archive or index'
        'verify:Verify a signature'
        'trust:Add a public key to the trust store'
        'upgrade:Upgrade installed packages from a repository'
        'rollback:Roll a package back to the previous version'
        'fetch:Download and verify external sources, then run the fetch backends'
        'recipe:Inspect and check recipes (check: source-independent warnings)'
        'history:Show the package transaction history'
        'plugin:Manage and run plugins'
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
            build|deps|deps-all|info|files|remove|rollback|why|query|list|update|fetch)
                _describe -t packages 'package' pkgs ;;
            install)
                _describe -t packages 'package' pkgs
                _files ;;
            service)
                if (( CURRENT == 3 )); then
                    _values 'action' list enable disable start stop restart status
                else
                    _describe -t services 'service' \
                        services=($(nawa service list 2>/dev/null | awk 'NR>1{print $1}'))
                fi ;;
            config)
                _values 'action' show ;;
            recipe)
                _values 'action' check ;;
            completions)
                _values 'shell' bash zsh fish ;;
        esac
    fi
}
_nawa "$@"
"#;

pub(crate) const COMPLETION_FISH: &str = r#"# fish completion for nawa (generated by `nawa completions fish`)
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
complete -c nawa -n '__fish_use_subcommand' -a 'fetch' -d 'Download and verify sources, then run the fetch backends'
complete -c nawa -n '__fish_use_subcommand' -a 'recipe' -d 'Inspect and check recipes'
complete -c nawa -n '__fish_seen_subcommand_from recipe' -a 'check' -d 'Source-independent recipe warnings'
complete -c nawa -n '__fish_use_subcommand' -a 'files owner repo-create repo-update keygen sign verify trust upgrade rollback service config init completions'
# package names, completed dynamically
set -l pkg_cmds build install update deps deps-all info files remove rollback why query list
complete -c nawa -n "__fish_seen_subcommand_from $pkg_cmds" -a "(nawa __complete_pkgs 2>/dev/null)"
# service actions
complete -c nawa -n '__fish_seen_subcommand_from service; and not __fish_seen_subcommand_from list enable disable start stop restart status' -a 'list enable disable start stop restart status'
# config actions
complete -c nawa -n '__fish_seen_subcommand_from config' -a 'show'
# completion shells
complete -c nawa -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish'
"#;

pub(crate) fn completions_script(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => COMPLETION_BASH,
        Shell::Zsh => COMPLETION_ZSH,
        Shell::Fish => COMPLETION_FISH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn group_install_does_not_take_an_outer_database_lock() {
        let install = Cli::try_parse_from(["nawa", "group", "install", "dev"]).unwrap();
        assert!(lock_mode(&install.cmd).is_none());

        let remove = Cli::try_parse_from(["nawa", "group", "remove", "dev"]).unwrap();
        assert_eq!(lock_mode(&remove.cmd), Some(lock::Mode::Exclusive));
    }

    #[test]
    fn reserved_commands_cover_every_builtin_cli_command() {
        let command = Cli::command();
        for subcommand in command.get_subcommands() {
            let name = subcommand.get_name();
            assert!(
                crate::plugins::manifest::RESERVED_COMMANDS.contains(&name),
                "built-in command {name:?} is missing from RESERVED_COMMANDS"
            );
        }
    }

    #[test]
    fn bash_completion_script_passes_bash_syntax_check() {
        let output = std::process::Command::new("bash")
            .args(["-n", "-c", COMPLETION_BASH])
            .output()
            .expect("bash must be available to validate the generated completion script");
        assert!(
            output.status.success(),
            "generated Bash completion syntax is invalid: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
