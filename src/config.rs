//! Configuration resolution: `nawa.toml` files + environment variables +
//! command-line flags, with a single documented precedence.
//!
//! Per-key precedence (highest wins):
//!   1. command-line flag            (`--root /x`)
//!   2. environment variable         (`NAWA_ROOT=/x`)
//!   3. the nearest `nawa.toml`       (searched upward from the CWD, like
//!      git; `NAWA_CONFIG` names one directly)
//!   4. the system file              (`/etc/nawa/nawa.toml`; the directory can
//!      be moved with `NAWA_SYSCONFDIR`, e.g. for tests)
//!   5. built-in default
//!
//! Relative paths in a config file resolve against *that file's* directory,
//! so the file means the same thing from any subdirectory. Paths from flags
//! and the environment resolve against the CWD as usual.
//!
//! The built-in default repository is [`DEFAULT_REPO`]; `repo = ""` (or
//! `none`) in any layer switches binary repositories off entirely.
//!
//! Nothing here talks to the filesystem beyond finding + reading the files,
//! so it is trivially testable.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The config file name searched in the CWD and every ancestor, and the
/// name of the system-wide file inside the system config directory.
pub const CONFIG_FILE: &str = "nawa.toml";

/// Where the system-wide configuration lives (`/etc/nawa/nawa.toml`).
pub const SYSCONF_DIR: &str = "/etc/nawa";

/// The repository used when nothing else is configured. `index.toml`, the
/// optional `index.toml.sig` and the `*.nawa.tar.zst` archives are expected
/// directly under this URL.
pub const DEFAULT_REPO: &str =
    "https://github.com/mrdevmohamed/faseela-repo/releases/download/stable";

/// Where `nawa install <name>` gets a package from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    /// use the repository when it has the package (at least as new as the
    /// recipe), otherwise build it from the recipe — the default
    Auto,
    /// repository only: a package the repository lacks is an error
    Repo,
    /// always build from the recipes (the pre-repository behaviour)
    Build,
    /// like `auto`, but a terminal user is asked repo-or-build first
    Ask,
}

impl InstallMode {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(InstallMode::Auto),
            "repo" | "binary" => Ok(InstallMode::Repo),
            "build" | "source" => Ok(InstallMode::Build),
            "ask" => Ok(InstallMode::Ask),
            other => bail!("unknown install mode {other:?} (want auto | repo | build | ask)"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            InstallMode::Auto => "auto",
            InstallMode::Repo => "repo",
            InstallMode::Build => "build",
            InstallMode::Ask => "ask",
        }
    }
}

/// What the user passed on the command line (already `None` when absent —
/// the CLI declares no defaults for these, resolution happens here).
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub root: Option<PathBuf>,
    pub recipes: Option<PathBuf>,
    pub builddir: Option<PathBuf>,
    pub profiles: Option<PathBuf>,
    pub jobs: Option<u32>,
    pub repo: Option<String>,
    pub install_mode: Option<InstallMode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileCfg {
    root: Option<String>,
    recipes: Option<String>,
    builddir: Option<String>,
    profiles: Option<String>,
    jobs: Option<u32>,
    repo: Option<String>,
    install_mode: Option<String>,
}

/// One parsed config file plus the directory its relative paths anchor at.
struct Layer {
    path: PathBuf,
    base: PathBuf,
    cfg: FileCfg,
}

/// Fully resolved configuration plus where every value came from
/// (for `nawa config show`).
#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub recipes: PathBuf,
    pub builddir: PathBuf,
    pub profiles: PathBuf,
    pub jobs: Option<u32>,
    /// the binary repository (`None` = switched off with `repo = ""`)
    pub repo: Option<String>,
    pub install_mode: InstallMode,
    /// The project `nawa.toml` that was applied, if any.
    pub file: Option<PathBuf>,
    /// The system-wide file (`/etc/nawa/nawa.toml`) that was applied, if any.
    pub system_file: Option<PathBuf>,
    /// key -> human-readable source ("flag", "env NAWA_ROOT", "<path>", "default")
    pub sources: BTreeMap<String, String>,
}

impl Config {
    pub fn source_of(&self, key: &str) -> &str {
        self.sources.get(key).map(|s| s.as_str()).unwrap_or("default")
    }
}

/// The directory holding the system-wide configuration: `$NAWA_SYSCONFDIR`
/// if set, else [`SYSCONF_DIR`].
pub fn sysconf_dir() -> PathBuf {
    match std::env::var("NAWA_SYSCONFDIR") {
        Ok(s) if !s.trim().is_empty() => PathBuf::from(s.trim()),
        _ => PathBuf::from(SYSCONF_DIR),
    }
}

/// The system-wide config file (`/etc/nawa/nawa.toml` by default).
pub fn system_file() -> PathBuf {
    sysconf_dir().join(CONFIG_FILE)
}

/// Find the nearest `nawa.toml`: `$NAWA_CONFIG` if set, else the first file
/// named `nawa.toml` in the CWD or any of its ancestors.
pub fn discover() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("NAWA_CONFIG") {
        let p = PathBuf::from(p.trim());
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    let cwd = std::env::current_dir().ok()?;
    let mut dir = cwd.as_path();
    loop {
        let candidate = dir.join(CONFIG_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
}

fn read_layer(path: &Path) -> Result<Layer> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    let cfg: FileCfg =
        toml::from_str(&raw).with_context(|| format!("bad config {}", path.display()))?;
    let base = path.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    Ok(Layer { path: path.to_path_buf(), base, cfg })
}

/// First layer (highest precedence first) that sets the key.
fn pick<T>(
    layers: &[Layer],
    get: impl Fn(&FileCfg) -> Option<T>,
) -> Option<(T, &Layer)> {
    layers.iter().find_map(|l| get(&l.cfg).map(|v| (v, l)))
}

/// flag > env > config layers > default, for a path-valued key.
fn resolve_path(
    key: &str,
    flag: Option<PathBuf>,
    env: &str,
    layers: &[Layer],
    get: impl Fn(&FileCfg) -> Option<String>,
    default: PathBuf,
    sources: &mut BTreeMap<String, String>,
) -> PathBuf {
    if let Some(v) = flag {
        sources.insert(key.into(), "flag".into());
        return v;
    }
    if let Some(v) = env_path(env) {
        sources.insert(key.into(), format!("env {env}"));
        return v;
    }
    if let Some((v, layer)) = pick(layers, get) {
        sources.insert(key.into(), layer.path.display().to_string());
        return layer.base.join(v);
    }
    default
}

/// `""`, `none`, `off` and `disabled` mean "no repository".
fn normalize_repo(s: &str) -> Option<String> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "" | "none" | "off" | "disabled" => None,
        _ => Some(t.to_string()),
    }
}

/// Resolve the effective configuration from all layers.
pub fn load(ov: &Overrides) -> Result<Config> {
    let project_path = discover();
    let sys_path = system_file();

    // layers, highest precedence first: project file, then the system file
    let mut layers: Vec<Layer> = Vec::new();
    if let Some(p) = &project_path {
        layers.push(read_layer(p)?);
    }
    let mut system_applied: Option<PathBuf> = None;
    if sys_path.is_file() && project_path.as_deref() != Some(sys_path.as_path()) {
        layers.push(read_layer(&sys_path)?);
        system_applied = Some(sys_path.clone());
    }

    let mut cfg = Config {
        root: PathBuf::from("/"),
        recipes: PathBuf::from("recipes"),
        builddir: PathBuf::from("build"),
        profiles: PathBuf::from("profiles"),
        jobs: None,
        repo: Some(DEFAULT_REPO.to_string()),
        install_mode: InstallMode::Auto,
        file: project_path.clone(),
        system_file: system_applied,
        sources: BTreeMap::new(),
    };

    // ---- the four path keys -------------------------------------------------
    cfg.root = resolve_path(
        "root", ov.root.clone(), "NAWA_ROOT", &layers,
        |c| c.root.clone(), PathBuf::from("/"), &mut cfg.sources,
    );
    cfg.recipes = resolve_path(
        "recipes", ov.recipes.clone(), "NAWA_RECIPES", &layers,
        |c| c.recipes.clone(), PathBuf::from("recipes"), &mut cfg.sources,
    );
    cfg.builddir = resolve_path(
        "builddir", ov.builddir.clone(), "NAWA_BUILDDIR", &layers,
        |c| c.builddir.clone(), PathBuf::from("build"), &mut cfg.sources,
    );
    cfg.profiles = resolve_path(
        "profiles", ov.profiles.clone(), "NAWA_PROFILES", &layers,
        |c| c.profiles.clone(), PathBuf::from("profiles"), &mut cfg.sources,
    );

    // ---- jobs -------------------------------------------------------------
    if let Some(v) = ov.jobs {
        cfg.jobs = Some(v);
        cfg.sources.insert("jobs".into(), "flag".into());
    } else if let Some(v) = env_u32("NAWA_JOBS")? {
        cfg.jobs = Some(v);
        cfg.sources.insert("jobs".into(), "env NAWA_JOBS".into());
    } else if let Some((v, layer)) = pick(&layers, |c| c.jobs) {
        cfg.jobs = Some(v);
        cfg.sources.insert("jobs".into(), layer.path.display().to_string());
    }

    // ---- repo (built-in default: DEFAULT_REPO) -------------------------------
    if let Some(v) = ov.repo.clone() {
        cfg.repo = normalize_repo(&v);
        cfg.sources.insert("repo".into(), "flag".into());
    } else if let Some(v) = std::env::var("NAWA_REPO").ok().filter(|s| !s.trim().is_empty()) {
        cfg.repo = normalize_repo(&v);
        cfg.sources.insert("repo".into(), "env NAWA_REPO".into());
    } else if let Some((v, layer)) = pick(&layers, |c| c.repo.clone()) {
        cfg.repo = normalize_repo(&v);
        cfg.sources.insert("repo".into(), layer.path.display().to_string());
    }

    // ---- install_mode (default: auto) -----------------------------------------
    if let Some(m) = ov.install_mode {
        cfg.install_mode = m;
        cfg.sources.insert("install_mode".into(), "flag".into());
    } else if let Some(v) = std::env::var("NAWA_INSTALL_MODE").ok().filter(|s| !s.trim().is_empty()) {
        cfg.install_mode = InstallMode::parse(&v).context("bad NAWA_INSTALL_MODE")?;
        cfg.sources.insert("install_mode".into(), "env NAWA_INSTALL_MODE".into());
    } else if let Some((v, layer)) = pick(&layers, |c| c.install_mode.clone()) {
        cfg.install_mode = InstallMode::parse(&v)
            .with_context(|| format!("bad install_mode in {}", layer.path.display()))?;
        cfg.sources.insert("install_mode".into(), layer.path.display().to_string());
    }

    Ok(cfg)
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty()).map(PathBuf::from)
}

fn env_u32(key: &str) -> Result<Option<u32>> {
    match std::env::var(key) {
        Ok(s) if !s.trim().is_empty() => s
            .trim()
            .parse::<u32>()
            .map(Some)
            .with_context(|| format!("bad {key} (want a positive integer): {s:?}")),
        _ => Ok(None),
    }
}

/// The commented template `nawa init` writes (a per-project file).
pub fn template() -> String {
    format!(
        r#"# {CONFIG_FILE} — nawa configuration
# nawa reads the nearest {CONFIG_FILE} at or above the current directory
# (or the file named by $NAWA_CONFIG), then the system-wide /etc/nawa/{CONFIG_FILE}.
# Every key is optional.
# Precedence: command-line flag > environment variable > this file >
#             /etc/nawa/{CONFIG_FILE} > built-in default.
# Relative paths here resolve against this file's directory.

# Target root filesystem (env: NAWA_ROOT)
#root = "/"

# Recipes tree, scratch dir, build profiles (env: NAWA_RECIPES / NAWA_BUILDDIR / NAWA_PROFILES)
#recipes = "recipes"
#builddir = "build"
#profiles = "profiles"

# Default parallel build jobs (env: NAWA_JOBS)
#jobs = 8

# Binary repository for `nawa install` and `nawa upgrade` (env: NAWA_REPO) —
# local dir or http(s) URL. "" switches the repository off.
# Built-in default: {DEFAULT_REPO}
#repo = "{DEFAULT_REPO}"

# Where `nawa install <name>` gets packages (env: NAWA_INSTALL_MODE):
#   auto  - repository when it has the package, otherwise build  (default)
#   repo  - repository only (error if the package is not there)
#   build - always build from the recipes
#   ask   - like auto, but asks "repository or build?" in a terminal
#install_mode = "auto"
"#
    )
}

/// The file `nawa init --system` writes to `/etc/nawa/nawa.toml`, and the one
/// the `nawa` package ships: the repository and install mode are active, the
/// rest is commented.
pub fn system_template() -> String {
    format!(
        r#"# /etc/nawa/{CONFIG_FILE} — system-wide nawa configuration
#
# Precedence (highest wins): command-line flag > NAWA_* environment variable >
# the nearest {CONFIG_FILE} in or above the working directory > this file >
# built-in default.  Inspect the result with `nawa config show`.
# Relative paths in this file resolve against /etc/nawa.

# Default binary repository (env: NAWA_REPO).  A local directory or an
# http(s) URL that serves index.toml, index.toml.sig and the *.nawa.tar.zst
# archives.  Set it to "" to switch repositories off.
# Remote repositories need a trusted key (`nawa trust <key.pub>`) unless you
# pass --allow-unsigned.
repo = "{DEFAULT_REPO}"

# Where `nawa install <name>` gets packages (env: NAWA_INSTALL_MODE):
#   auto  - install from the repository when it has the package, otherwise
#           build it from the recipe (default)
#   repo  - repository only; a package the repository lacks is an error
#   build - always build from source
#   ask   - like auto, but asks "repository or build?" in a terminal
# Per command: `nawa install --build foo` / `--from-repo foo` / `--from ask foo`.
install_mode = "auto"

# Recipes used for building and for dependency resolution (env: NAWA_RECIPES).
# `nawa sync` installs and updates the recipe tree here (signed, from the repository).
recipes = "/usr/share/nawa/recipes"
#builddir = "/var/tmp/nawa-build"
#profiles = "/etc/nawa/profiles"
#jobs = 8
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    // these tests mutate process-wide state (CWD + env vars) — serialize
    // them, cargo runs a binary's tests on parallel threads
    static CFG_LOCK: Mutex<()> = Mutex::new(());

    fn isolate(dir: &Path) -> MutexGuard<'static, ()> {
        let guard = CFG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_current_dir(dir).unwrap();
        // never read the developer's real /etc/nawa/nawa.toml
        std::env::set_var("NAWA_SYSCONFDIR", dir.join("__no-sysconf__"));
        std::env::remove_var("NAWA_REPO");
        std::env::remove_var("NAWA_INSTALL_MODE");
        guard
    }

    fn restore(_guard: MutexGuard<'static, ()>, dir: &Path) {
        std::env::set_current_dir(dir).unwrap();
        drop(_guard);
    }

    fn tempdir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("nawa-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn defaults_without_any_config() {
        let dir = tempdir("plain");
        let g = isolate(&dir);
        std::env::remove_var("NAWA_CONFIG");
        let cfg = load(&Overrides::default()).unwrap();
        restore(g, &dir);
        assert_eq!(cfg.root, PathBuf::from("/"));
        assert_eq!(cfg.recipes, PathBuf::from("recipes"));
        assert_eq!(cfg.builddir, PathBuf::from("build"));
        assert_eq!(cfg.profiles, PathBuf::from("profiles"));
        assert_eq!(cfg.jobs, None);
        assert_eq!(cfg.repo.as_deref(), Some(DEFAULT_REPO), "built-in default repo");
        assert_eq!(cfg.install_mode, InstallMode::Auto);
        assert_eq!(cfg.source_of("root"), "default");
        assert_eq!(cfg.source_of("repo"), "default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_discovery_walks_up_and_anchors_relative_paths() {
        let base = tempdir("up");
        let project = base.join("project");
        let deep = project.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(
            project.join("nawa.toml"),
            "recipes = \"src/recipes\"\njobs = 4\n",
        )
        .unwrap();
        let g = isolate(&deep);
        std::env::remove_var("NAWA_CONFIG");
        let found = discover().unwrap();
        assert_eq!(found, project.join("nawa.toml"));
        let cfg = load(&Overrides::default()).unwrap();
        restore(g, &deep);
        // relative to the FILE, not the CWD
        assert_eq!(cfg.recipes, project.join("src/recipes"));
        assert_eq!(cfg.jobs, Some(4));
        assert_eq!(cfg.source_of("jobs"), project.join("nawa.toml").to_string_lossy());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn precedence_flag_beats_env_beats_file() {
        let base = tempdir("prec");
        std::fs::write(base.join("nawa.toml"), "jobs = 2\nbuilddir = \"from-file\"\n").unwrap();
        let g = isolate(&base);
        std::env::remove_var("NAWA_CONFIG");
        std::env::set_var("NAWA_JOBS", "6");
        let ov = Overrides {
            jobs: Some(9),
            builddir: Some(PathBuf::from("from-flag")),
            ..Default::default()
        };
        let cfg = load(&ov).unwrap();
        assert_eq!(cfg.jobs, Some(9), "flag wins");
        assert_eq!(cfg.builddir, PathBuf::from("from-flag"), "flag wins");
        std::env::remove_var("NAWA_JOBS");
        let cfg = load(&Overrides::default()).unwrap();
        assert_eq!(cfg.jobs, Some(2), "env > file");
        std::env::set_var("NAWA_JOBS", "6");
        let cfg = load(&Overrides::default()).unwrap();
        assert_eq!(cfg.jobs, Some(6));
        assert_eq!(cfg.builddir, base.join("from-file"));
        std::env::remove_var("NAWA_JOBS");
        restore(g, &base);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn unknown_keys_are_rejected_loudly() {
        let base = tempdir("bad");
        std::fs::write(base.join("nawa.toml"), "jobss = 8\n").unwrap();
        let g = isolate(&base);
        std::env::remove_var("NAWA_CONFIG");
        let err = load(&Overrides::default()).unwrap_err();
        restore(g, &base);
        // the toml cause rides along the anyhow chain ("{:#}")
        assert!(format!("{err:#}").contains("unknown field"), "{err:#}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn nawa_config_env_points_at_a_file() {
        let base = tempdir("env");
        let dir = base.join("elsewhere");
        std::fs::create_dir_all(&dir).unwrap();
        let cfgfile = base.join("custom.toml");
        std::fs::write(&cfgfile, "repo = \"https://example.com/repo\"\n").unwrap();
        let g = isolate(&dir);
        std::env::set_var("NAWA_CONFIG", &cfgfile);
        let cfg = load(&Overrides::default()).unwrap();
        std::env::remove_var("NAWA_CONFIG");
        restore(g, &dir);
        assert_eq!(cfg.repo.as_deref(), Some("https://example.com/repo"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn system_file_is_a_layer_below_the_project_file() {
        let base = tempdir("sys");
        let sysdir = base.join("etc-nawa");
        std::fs::create_dir_all(&sysdir).unwrap();
        std::fs::write(
            sysdir.join("nawa.toml"),
            "jobs = 3\nrecipes = \"sysrecipes\"\nrepo = \"https://sys.example/repo\"\ninstall_mode = \"build\"\n",
        )
        .unwrap();
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("nawa.toml"), "jobs = 5\n").unwrap();

        let g = isolate(&project);
        std::env::set_var("NAWA_SYSCONFDIR", &sysdir);
        std::env::remove_var("NAWA_CONFIG");
        let cfg = load(&Overrides::default()).unwrap();
        restore(g, &project);

        // project file wins where both set a key ...
        assert_eq!(cfg.jobs, Some(5));
        assert_eq!(cfg.source_of("jobs"), project.join("nawa.toml").to_string_lossy());
        // ... the system file fills in everything else, relative paths
        // anchored at /etc/nawa (here: the temp sysconf dir)
        assert_eq!(cfg.recipes, sysdir.join("sysrecipes"));
        assert_eq!(cfg.repo.as_deref(), Some("https://sys.example/repo"));
        assert_eq!(cfg.install_mode, InstallMode::Build);
        assert_eq!(cfg.source_of("repo"), sysdir.join("nawa.toml").to_string_lossy());
        assert_eq!(cfg.system_file, Some(sysdir.join("nawa.toml")));
        assert_eq!(cfg.file, Some(project.join("nawa.toml")));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn env_and_flag_beat_the_files_for_repo_and_install_mode() {
        let base = tempdir("mode");
        std::fs::write(
            base.join("nawa.toml"),
            "repo = \"https://file.example/r\"\ninstall_mode = \"repo\"\n",
        )
        .unwrap();
        let g = isolate(&base);
        std::env::remove_var("NAWA_CONFIG");
        std::env::set_var("NAWA_INSTALL_MODE", "ask");
        std::env::set_var("NAWA_REPO", "https://env.example/r");
        let cfg = load(&Overrides::default()).unwrap();
        assert_eq!(cfg.install_mode, InstallMode::Ask, "env > file");
        assert_eq!(cfg.repo.as_deref(), Some("https://env.example/r"));
        let ov = Overrides {
            install_mode: Some(InstallMode::Build),
            repo: Some("/srv/local-repo".into()),
            ..Default::default()
        };
        let cfg2 = load(&ov).unwrap();
        std::env::remove_var("NAWA_INSTALL_MODE");
        std::env::remove_var("NAWA_REPO");
        restore(g, &base);
        assert_eq!(cfg2.install_mode, InstallMode::Build, "flag > env");
        assert_eq!(cfg2.repo.as_deref(), Some("/srv/local-repo"));
        assert_eq!(cfg2.source_of("repo"), "flag");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn empty_repo_switches_the_repository_off() {
        let base = tempdir("norepo");
        std::fs::write(base.join("nawa.toml"), "repo = \"\"\n").unwrap();
        let g = isolate(&base);
        std::env::remove_var("NAWA_CONFIG");
        let cfg = load(&Overrides::default()).unwrap();
        restore(g, &base);
        assert_eq!(cfg.repo, None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn bad_install_mode_is_rejected_loudly() {
        let base = tempdir("badmode");
        std::fs::write(base.join("nawa.toml"), "install_mode = \"sometimes\"\n").unwrap();
        let g = isolate(&base);
        std::env::remove_var("NAWA_CONFIG");
        let err = load(&Overrides::default()).unwrap_err();
        restore(g, &base);
        assert!(format!("{err:#}").contains("unknown install mode"), "{err:#}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_mode_parses_aliases() {
        assert_eq!(InstallMode::parse("Auto").unwrap(), InstallMode::Auto);
        assert_eq!(InstallMode::parse("binary").unwrap(), InstallMode::Repo);
        assert_eq!(InstallMode::parse("source").unwrap(), InstallMode::Build);
        assert!(InstallMode::parse("x").is_err());
    }

    #[test]
    fn shipped_system_template_parses() {
        let base = tempdir("tpl");
        std::fs::write(base.join("nawa.toml"), system_template()).unwrap();
        let g = isolate(&base);
        std::env::remove_var("NAWA_CONFIG");
        let cfg = load(&Overrides::default()).unwrap();
        restore(g, &base);
        assert_eq!(cfg.repo.as_deref(), Some(DEFAULT_REPO));
        assert_eq!(cfg.install_mode, InstallMode::Auto);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn shipped_dist_file_matches_the_system_template() {
        assert_eq!(
            include_str!("../dist/nawa.toml"),
            system_template(),
            "pkgmgr/dist/nawa.toml (installed as /etc/nawa/nawa.toml) drifted from config::system_template()"
        );
    }
}
