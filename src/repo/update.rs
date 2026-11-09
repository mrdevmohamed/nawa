//! Index updating.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use super::model::*;
use super::create::write_index_atomic;
use super::validate::*;
use super::pack::pack_recipes;

/// What changed since a base index (see `update_index`).
#[derive(Debug, Default)]
pub(crate) struct IndexDelta {
    /// Entries for new archives, one per archive, in any order.
    pub add: Vec<RepoEntry>,
    /// Archive file names that left the repository. An unindexed name (an old
    /// archive that was never the newest) is a no-op.
    pub remove_files: Vec<String>,
    /// Package names whose entry disappears on purpose (every archive of the
    /// package is gone).
    pub drop_packages: Vec<String>,
}

fn archive_file_name(e: &RepoEntry) -> String {
    format!("{}-{}-{}.nawa.tar.zst", e.name, e.version, e.rel)
}

/// Apply a delta to a schema-2/3 base index WITHOUT opening any archive.
///
/// The caller must already have verified the base index signature. For every
/// delta this equals `repo-create` over (archives of the base − removed + added)
/// as long as removals follow the contract below, which is why a removal that
/// would silently lose or expose a package is an error:
///
/// * removing the indexed archive of a package needs an added entry for that
///   package (the replacement, possibly an older archive that becomes the
///   newest) or an explicit `drop_packages` entry;
/// * the newest archive per package name wins (`vercmp` of version-rel), exactly
///   like repo-create; an older added entry is ignored;
/// * an archive file name is immutable: re-adding the same name with another
///   sha256 or size is an error, re-adding it unchanged is a no-op.
pub(crate) fn update_index(
    base: &Index,
    delta: IndexDelta,
    recipes: Option<RecipesEntry>,
    now: u64,
) -> Result<Index> {
    let meta = base.repository.clone().context(
        "repo-update needs a schema-2 or schema-3 base index; a v1 index has no signed hash policy (use repo-create)",
    )?;
    let mut map: BTreeMap<String, RepoEntry> = BTreeMap::new();
    for p in &base.packages {
        if map.insert(p.name.clone(), p.clone()).is_some() {
            bail!("base index contains duplicate package name {:?}", p.name);
        }
    }

    let mut removed_names: HashSet<String> = HashSet::new();
    for file in &delta.remove_files {
        validate_plain_filename(file)?;
        if let Some(name) = map.values().find(|p| &p.file == file).map(|p| p.name.clone()) {
            map.remove(&name);
            removed_names.insert(name);
        }
    }
    let mut dropped: HashSet<String> = HashSet::new();
    for name in &delta.drop_packages {
        if map.remove(name).is_none() && !removed_names.contains(name) {
            bail!("cannot drop {name:?}: it is not in the base index");
        }
        dropped.insert(name.clone());
    }

    for e in delta.add {
        let expected = archive_file_name(&e);
        if e.file != expected {
            bail!("entry for {} has file {:?}, expected {:?}", e.name, e.file, expected);
        }
        match map.get(&e.name) {
            Some(cur) if cur.file == e.file => {
                if cur.sha256 != e.sha256 || cur.size != e.size {
                    bail!("{} is already indexed with a different sha256/size; archive files are immutable", e.file);
                }
            }
            Some(cur) => {
                let new_full = format!("{}-{}", e.version, e.rel);
                let cur_full = format!("{}-{}", cur.version, cur.rel);
                if crate::version::vercmp(&new_full, &cur_full) == std::cmp::Ordering::Greater {
                    map.insert(e.name.clone(), e);
                }
            }
            None => {
                map.insert(e.name.clone(), e);
            }
        }
    }

    for name in &removed_names {
        if !dropped.contains(name) && !map.contains_key(name) {
            bail!(
                "removing the indexed archive of {name} would leave it without an entry; \
                 add the archive that replaces it, or drop the package explicitly"
            );
        }
    }

    let index = Index {
        generated: Some(now),
        expires: Some(now + INDEX_VALIDITY_SECS),
        repository: Some(meta),
        recipes: recipes.or_else(|| base.recipes.clone()),
        groups: base.groups.clone(),
        packages: map.into_values().collect(),
    };
    index.validate()?;
    Ok(index)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntriesFile {
    #[serde(rename = "package", default)]
    packages: Vec<RepoEntry>,
}

/// Optional inputs for the file-level `repo-update` flow, mirroring the CLI
/// flags: entries to add, archives/packages to remove, an archive-locations
/// manifest, and a new recipe snapshot.
#[derive(Debug, Default)]
pub(crate) struct UpdateIndexFilesInputs<'a> {
    pub entries_path: Option<&'a Path>,
    pub remove_files: Vec<String>,
    pub drop_packages: Vec<String>,
    pub locations_path: Option<&'a Path>,
    pub recipes: Option<(&'a Path, &'a str)>,
}

/// File-level `repo-update`: read the delta inputs, apply `update_index`, and
/// write the new index atomically. `dir` receives the recipe snapshot when
/// `inputs.recipes` is given (as in repo-create); without it the base recipes
/// entry is kept unchanged. Signing is the caller's job.
pub(crate) fn update_index_files(
    base: &Index,
    dir: &Path,
    out: &Path,
    inputs: UpdateIndexFilesInputs<'_>,
) -> Result<Index> {
    let UpdateIndexFilesInputs {
        entries_path,
        remove_files,
        drop_packages,
        locations_path,
        recipes,
    } = inputs;
    let schema = base.repository.as_ref().map(|m| m.schema).unwrap_or(1);
    if schema != INDEX_SCHEMA_V3 && locations_path.is_some() {
        bail!("--locations is only valid with a schema-3 base index");
    }
    let locations = match locations_path {
        Some(path) => Some(load_locations(path)?),
        None => None,
    };
    let mut add = match entries_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("read entries {}", path.display()))?;
            let parsed: EntriesFile = toml::from_str(&raw)
                .with_context(|| format!("parse entries {}", path.display()))?;
            parsed.packages
        }
        None => Vec::new(),
    };
    if let Some(loc) = &locations {
        for e in &mut add {
            if e.download_url.is_none() {
                e.download_url = loc.package_urls.get(&e.file).cloned();
            }
        }
    }
    let mut recipes_entry = match recipes {
        Some((tree, rev)) => Some(pack_recipes(tree, dir, rev)?),
        None => None,
    };
    if let (Some(entry), Some(loc)) = (&mut recipes_entry, &locations) {
        entry.download_url = loc.recipe_urls.get(&entry.file).cloned();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let index = update_index(
        base,
        IndexDelta { add, remove_files, drop_packages },
        recipes_entry,
        now,
    )?;
    write_index_atomic(&index, out)?;
    crate::nawa_info!("wrote index {} ({} packages, schema v{})", out.display(), index.packages.len(), schema);
    Ok(index)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocationsFile {
    schema: u32,
    #[serde(default)]
    pub(crate) package_urls: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) recipe_urls: BTreeMap<String, String>,
}

pub(crate) fn load_locations(path: &Path) -> Result<LocationsFile> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read archive locations {}", path.display()))?;
    let locations: LocationsFile = toml::from_str(&raw)
        .with_context(|| format!("parse archive locations {}", path.display()))?;
    if locations.schema != 1 {
        bail!("unsupported archive locations schema {}; expected 1", locations.schema);
    }
    for (file, url) in locations.package_urls.iter().chain(locations.recipe_urls.iter()) {
        validate_plain_filename(file)?;
        validate_download_url(url, &format!("archive {file}"))?;
    }
    Ok(locations)
}

