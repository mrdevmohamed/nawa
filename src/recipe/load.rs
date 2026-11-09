//! Recipe loading.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walkdir;

use super::model::*;

pub(crate) fn load_all(root: &Path) -> Result<BTreeMap<String, Recipe>> {
    let mut map = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() == "recipe.toml")
    {
        let rec = Recipe::load(entry.path())?;
        let name = rec.package.name.clone();
        if map.insert(name.clone(), rec).is_some() {
            // name the package that duplicated, not whatever happens to be
            // the alphabetically-last key of the map so far
            bail!("duplicate recipe name under {}: `{}`", root.display(), name);
        }
    }
    Ok(map)
}

/// Load one recipe by package name, searching `recipes/**/recipe.toml`.
/// A miss suggests close recipe names — "did you mean `gzip`?" beats a bare
/// "not found" every time.
pub(crate) fn load_by_name(root: &Path, name: &str) -> Result<Recipe> {
    Ok(locate(root, name)?.0)
}

/// Load one recipe by name and also report where its `recipe.toml` lives
/// (single walk — `load_by_name` drops the path).
pub(crate) fn locate(root: &Path, name: &str) -> Result<(Recipe, PathBuf)> {
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() == "recipe.toml")
    {
        let rec = Recipe::load(entry.path())?;
        if rec.package.name == name {
            return Ok((rec, entry.path().to_path_buf()));
        }
    }
    // miss: offer the closest names we know about (best effort — a tree that
    // fails to parse still reports the plain "not found" below)
    let names: Vec<String> = load_all(root)
        .map(|m| m.into_keys().collect())
        .unwrap_or_default();
    bail!(
        "recipe not found: `{}` (searched {}){}",
        name,
        root.display(),
        crate::suggest::hint(name, &names)
    )
}

