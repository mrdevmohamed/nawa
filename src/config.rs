//! Configuration resolution: `nawa.toml` file + environment variables +
//! command-line flags, with a single documented precedence.
//!
//! Per-key precedence (highest wins):
//!   1. command-line flag            (`--root /x`)
//!   2. environment variable         (`NAWA_ROOT=/x`)
//!   3. the nearest `nawa.toml`       (searched upward from the CWD, like
//!      git; `NAWA_CONFIG` names one directly)
//!   4. built-in default
//!
//! Relative paths in `nawa.toml` resolve against the *file's* directory, so
//! the file means the same thing from any subdirectory. Paths from flags and
//! the environment resolve against the CWD as usual.
//!
//! Nothing here talks to the filesystem beyond finding + reading the file,
//! so it is trivially testable.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The config file name searched in the CWD and every ancestor.
pub const CONFIG_FILE: &str = "nawa.toml";

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
    pub repo: Option<String>,
    /// The file that was applied, if any.
    pub file: Option<PathBuf>,
    /// key -> human-readable source ("flag", "env NAWA_ROOT", "<path>", "default")
    pub sources: BTreeMap<String, String>,
}

impl Config {
    pub fn source_of(&self, key: &str) -> &str {
        self.sources.get(key).map(|s| s.as_str()).unwrap_or("default")
    }
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

/// Resolve the effective configuration from all four layers.
pub fn load(ov: &Overrides) -> Result<Config> {
    let file_path = discover();
    let file: FileCfg = match &file_path {
        Some(p) => {
            let raw = std::fs::read_to_string(p)
                .with_context(|| format!("cannot read config {}", p.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("bad config {}", p.display()))?
        }
        None => FileCfg::default(),
    };
    // relative paths inside nawa.toml are anchored at the file's directory
    let base: PathBuf = file_path
        .as_ref()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();

    let mut cfg = Config {
        root: PathBuf::from("/"),
        recipes: PathBuf::from("recipes"),
        builddir: PathBuf::from("build"),
        profiles: PathBuf::from("profiles"),
        jobs: None,
        repo: None,
        file: file_path.clone(),
        sources: BTreeMap::new(),
    };

    // ---- root -------------------------------------------------------------
    if let Some(v) = ov.root.clone() {
        cfg.root = v;
        cfg.sources.insert("root".into(), "flag".into());
    } else if let Some(v) = env_path("NAWA_ROOT") {
        cfg.root = v;
        cfg.sources.insert("root".into(), "env NAWA_ROOT".into());
    } else if let Some(v) = file.root.clone() {
        cfg.root = base.join(v);
        cfg.sources.insert("root".into(), file_label(&file_path));
    }

    // ---- recipes ----------------------------------------------------------
    if let Some(v) = ov.recipes.clone() {
        cfg.recipes = v;
        cfg.sources.insert("recipes".into(), "flag".into());
    } else if let Some(v) = env_path("NAWA_RECIPES") {
        cfg.recipes = v;
        cfg.sources.insert("recipes".into(), "env NAWA_RECIPES".into());
    } else if let Some(v) = file.recipes.clone() {
        cfg.recipes = base.join(v);
        cfg.sources.insert("recipes".into(), file_label(&file_path));
    }

    // ---- builddir ---------------------------------------------------------
    if let Some(v) = ov.builddir.clone() {
        cfg.builddir = v;
        cfg.sources.insert("builddir".into(), "flag".into());
    } else if let Some(v) = env_path("NAWA_BUILDDIR") {
        cfg.builddir = v;
        cfg.sources.insert("builddir".into(), "env NAWA_BUILDDIR".into());
    } else if let Some(v) = file.builddir.clone() {
        cfg.builddir = base.join(v);
        cfg.sources.insert("builddir".into(), file_label(&file_path));
    }

    // ---- profiles ---------------------------------------------------------
    if let Some(v) = ov.profiles.clone() {
        cfg.profiles = v;
        cfg.sources.insert("profiles".into(), "flag".into());
    } else if let Some(v) = env_path("NAWA_PROFILES") {
        cfg.profiles = v;
        cfg.sources.insert("profiles".into(), "env NAWA_PROFILES".into());
    } else if let Some(v) = file.profiles.clone() {
        cfg.profiles = base.join(v);
        cfg.sources.insert("profiles".into(), file_label(&file_path));
    }

    // ---- jobs -------------------------------------------------------------
    if let Some(v) = ov.jobs {
        cfg.jobs = Some(v);
        cfg.sources.insert("jobs".into(), "flag".into());
    } else if let Some(v) = env_u32("NAWA_JOBS")? {
        cfg.jobs = Some(v);
        cfg.sources.insert("jobs".into(), "env NAWA_JOBS".into());
    } else if let Some(v) = file.jobs {
        cfg.jobs = Some(v);
        cfg.sources.insert("jobs".into(), file_label(&file_path));
    }

    // ---- repo -------------------------------------------------------------
    if let Some(v) = ov.repo.clone() {
        cfg.repo = Some(v);
        cfg.sources.insert("repo".into(), "flag".into());
    } else if let Some(v) = std::env::var("NAWA_REPO").ok().filter(|s| !s.trim().is_empty()) {
        cfg.repo = Some(v.trim().to_string());
        cfg.sources.insert("repo".into(), "env NAWA_REPO".into());
    } else if let Some(v) = file.repo.clone() {
        cfg.repo = Some(v);
        cfg.sources.insert("repo".into(), file_label(&file_path));
    }

    Ok(cfg)
}

fn file_label(file_path: &Option<PathBuf>) -> String {
    match file_path {
        Some(p) => format!("{}", p.display()),
        None => CONFIG_FILE.to_string(),
    }
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

/// The commented template `nawa init` writes.
pub fn template() -> String {
    format!(
        r#"# {CONFIG_FILE} — nawa configuration
# nawa reads the nearest {CONFIG_FILE} at or above the current directory
# (or the file named by $NAWA_CONFIG). Every key is optional.
# Precedence: command-line flag > environment variable > this file > default.
# Relative paths here resolve against this file's directory.

# Target root filesystem (env: NAWA_ROOT)
#root = "/"

# Recipes tree, scratch dir, build profiles (env: NAWA_RECIPES / NAWA_BUILDDIR / NAWA_PROFILES)
#recipes = "recipes"
#builddir = "build"
#profiles = "profiles"

# Default parallel build jobs (env: NAWA_JOBS)
#jobs = 8

# Default repository for `nawa upgrade` (env: NAWA_REPO) — local dir or http(s) URL
#repo = "https://mirror.example.com/mydistro/stable"
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
        assert_eq!(cfg.repo, None);
        assert_eq!(cfg.source_of("root"), "default");
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
}
