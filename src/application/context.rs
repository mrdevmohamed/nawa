//! Per-command context: the resolved configuration plus the small helpers
//! every command handler needs (recipe cache, install mode, repository).

use anyhow::{bail, Result};
use std::path::PathBuf;

use crate::config::{Config, InstallMode};

pub(crate) struct Ctx {
    pub(crate) root: PathBuf,
    pub(crate) recipes: PathBuf,
    pub(crate) builddir: PathBuf,
    pub(crate) profiles: PathBuf,
    pub(crate) jobs: Option<u32>,
    pub(crate) repo: Option<String>,
    pub(crate) install_mode: InstallMode,
    /// the resolved configuration, kept for `config show` (sources + file)
    pub(crate) cfg: Config,
    /// Per-command cache for `recipe::load_all(&recipes)` — cached after the
    /// first successful load so repeated lookups in the same `nawa` process
    /// (install/upgrade/build each walk the recipes tree multiple times)
    /// do not re-parse every `recipe.toml`. Safe because the recipes tree
    /// does not change during a single command execution.
    pub(crate) recipe_cache: std::cell::RefCell<Option<std::collections::BTreeMap<String, crate::recipe::Recipe>>>,
}

impl Ctx {
    pub(crate) fn from_config(cfg: &Config) -> Self {
        Ctx {
            root: cfg.root.clone(),
            recipes: cfg.recipes.clone(),
            builddir: cfg.builddir.clone(),
            profiles: cfg.profiles.clone(),
            jobs: cfg.jobs,
            repo: cfg.repo.clone(),
            install_mode: cfg.install_mode,
            cfg: cfg.clone(),
            recipe_cache: std::cell::RefCell::new(None),
        }
    }

    /// Cached [`recipe::load_all`] for this command's recipes root.
    pub(crate) fn load_all(&self) -> Result<std::collections::BTreeMap<String, crate::recipe::Recipe>> {
        let cached = self.recipe_cache.borrow();
        if let Some(map) = cached.as_ref() {
            return Ok(map.clone());
        }
        drop(cached);
        let loaded = crate::recipe::load_all(&self.recipes)?;
        *self.recipe_cache.borrow_mut() = Some(loaded.clone());
        Ok(loaded)
    }
}

/// `--build` / `--from-repo` / `--from` collapse to one install mode.
/// Shared by `install` and `update` so flag precedence cannot drift.
pub(crate) fn resolve_mode(build: bool, from_repo: bool, from: Option<&str>) -> Result<Option<InstallMode>> {
    if build {
        Ok(Some(InstallMode::Build))
    } else if from_repo {
        Ok(Some(InstallMode::Repo))
    } else if let Some(m) = from {
        Ok(Some(InstallMode::parse(m)?))
    } else {
        Ok(None)
    }
}

/// The configured repository (flag beats config), parsed.
/// Shared by `upgrade` and `sync` so the "no repository" bail text
/// cannot drift.
pub(crate) fn resolve_repo(ctx: &Ctx, repo: Option<String>) -> Result<crate::fetch::RepoRef> {
    let repo = match repo.or_else(|| ctx.repo.clone()) {
        Some(r) => r,
        None => bail!(
            "no repository configured (it is switched off with `repo = \"\"`): \
             pass --repo <dir-or-url>, set `repo` in nawa.toml or \
             /etc/nawa/nawa.toml, or export NAWA_REPO"
        ),
    };
    crate::fetch::RepoRef::parse(&repo)
}
